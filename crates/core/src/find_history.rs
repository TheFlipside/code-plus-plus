//! Rolling Find/Replace dialog history.
//!
//! The dialog populates its combobox dropdowns from this on every
//! open and pushes the typed query (and replacement) after each
//! successful Find Next / Replace operation. Persisted to
//! `find_history.xml` next to `session.xml` so it survives across
//! launches.
//!
//! Schema (stable from Phase 4 m3b2c onward):
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <find-history>
//!   <find>regex pattern</find>
//!   <find>another query</find>
//!   <replace>replacement</replace>
//! </find-history>
//! ```
//!
//! Order is most-recent-first (LRU). Re-pushing an existing entry
//! moves it to the front.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Maximum number of distinct entries kept in each list. Older
/// entries fall off the back when this is exceeded. The number
/// matches Notepad++'s default and keeps the file tiny enough
/// that an eager save-on-every-push is unnoticeable.
pub const MAX_ENTRIES: usize = 20;

/// Recent Find Next queries and Replace With strings, separately
/// keyed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "find-history")]
pub struct FindHistory {
    /// Most-recent-first list of search queries.
    #[serde(rename = "find", default)]
    pub finds: Vec<String>,
    /// Most-recent-first list of replacement strings.
    #[serde(rename = "replace", default)]
    pub replaces: Vec<String>,
}

/// Errors from reading or writing `find_history.xml`. Mirrors the
/// shape of [`crate::session::SessionError`] so callers can use
/// the same error-handling pattern.
#[derive(Debug)]
pub enum FindHistoryError {
    /// I/O error reading or writing the file.
    Io(std::io::Error),
    /// The file existed but didn't parse — corrupt or wrong
    /// schema. The file is dropped and a fresh empty history is
    /// used (callers should log + ignore).
    Parse(quick_xml::DeError),
    /// Serialization failure. Shouldn't happen with the simple
    /// String contents we produce.
    Serialize(quick_xml::SeError),
}

impl std::fmt::Display for FindHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FindHistoryError::Io(e) => write!(f, "find_history.xml I/O: {e}"),
            FindHistoryError::Parse(e) => write!(f, "find_history.xml parse: {e}"),
            FindHistoryError::Serialize(e) => write!(f, "find_history.xml serialize: {e}"),
        }
    }
}

impl std::error::Error for FindHistoryError {}

impl FindHistory {
    /// Push a new Find Next query to the front of the list. No-op
    /// for empty input; deduplicates (an existing entry is moved
    /// to the front rather than appended a second time); caps at
    /// [`MAX_ENTRIES`]. Returns `true` if the list actually
    /// changed — the Shell uses this to skip the disk write when
    /// the user is hammering Find Next on the same query (the
    /// most common Find pattern).
    pub fn push_find(&mut self, query: &str) -> bool {
        push_dedup(&mut self.finds, query)
    }

    /// Same shape as [`Self::push_find`] but for Replace With
    /// strings. Empty replacement (delete) is a valid Replace
    /// operation; an empty entry is still skipped here so the
    /// dropdown doesn't list a "blank" line that already exists
    /// implicitly when the field is empty.
    pub fn push_replace(&mut self, replacement: &str) -> bool {
        push_dedup(&mut self.replaces, replacement)
    }

    /// Read the XML file at `path`. Returns an empty history if
    /// the file does not exist; surfaces I/O / parse errors for
    /// any other failure mode.
    pub fn load(path: &Path) -> Result<Self, FindHistoryError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(FindHistoryError::Io(e)),
        };
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        quick_xml::de::from_str(&raw).map_err(FindHistoryError::Parse)
    }

    /// Atomic write to `path` via [`tempfile::NamedTempFile`] —
    /// the same shape `Session::save_to_xml` uses. Three
    /// guarantees:
    ///
    ///   - **Power-loss safety:** `sync_all` flushes the new
    ///     content to stable storage before the rename, so a
    ///     mid-save crash leaves either the old `find_history.xml`
    ///     intact or the new one fully written.
    ///   - **No stale tmp files:** dropping the `NamedTempFile`
    ///     on error removes it.
    ///   - **No TOCTOU substitution:** the tmp path is randomised
    ///     and owner-only, so a local actor can't pre-create a
    ///     symlink at a guessable sibling path to redirect the
    ///     write.
    pub fn save(&self, path: &Path) -> Result<(), FindHistoryError> {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        quick_xml::se::to_writer(&mut xml, self).map_err(FindHistoryError::Serialize)?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent).map_err(FindHistoryError::Io)?;
        }
        let parent_dir = parent.unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".find-history-")
            .suffix(".xml.tmp")
            .tempfile_in(parent_dir)
            .map_err(FindHistoryError::Io)?;
        tmp.write_all(xml.as_bytes())
            .map_err(FindHistoryError::Io)?;
        tmp.as_file_mut().sync_all().map_err(FindHistoryError::Io)?;
        tmp.persist(path)
            .map_err(|e| FindHistoryError::Io(e.error))?;
        Ok(())
    }
}

fn push_dedup(list: &mut Vec<String>, item: &str) -> bool {
    if item.is_empty() {
        return false;
    }
    // If the item is already at the front the list is already
    // in the desired state — skip the work AND signal "no
    // change" to the caller so it can skip its disk write.
    if list.first().map(String::as_str) == Some(item) {
        return false;
    }
    list.retain(|s| s != item);
    list.insert(0, item.to_string());
    if list.len() > MAX_ENTRIES {
        list.truncate(MAX_ENTRIES);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_dedup_moves_existing_entry_to_front() {
        let mut h = FindHistory::default();
        h.push_find("alpha");
        h.push_find("beta");
        h.push_find("alpha");
        assert_eq!(h.finds, vec!["alpha", "beta"]);
    }

    #[test]
    fn push_dedup_caps_at_max_entries() {
        let mut h = FindHistory::default();
        for i in 0..(MAX_ENTRIES + 5) {
            h.push_find(&format!("q{i}"));
        }
        assert_eq!(h.finds.len(), MAX_ENTRIES);
        // Most-recent first: the last pushed is index 0.
        assert_eq!(h.finds[0], format!("q{}", MAX_ENTRIES + 4));
    }

    #[test]
    fn push_returns_false_when_item_already_at_front() {
        let mut h = FindHistory::default();
        assert!(h.push_find("alpha"));
        assert!(!h.push_find("alpha"), "duplicate of front item is a no-op");
        assert!(h.push_find("beta"));
        assert!(
            h.push_find("alpha"),
            "moving an existing entry to the front IS a change"
        );
    }

    #[test]
    fn push_skips_empty() {
        let mut h = FindHistory::default();
        assert!(!h.push_find(""));
        assert!(!h.push_replace(""));
        assert!(h.finds.is_empty());
        assert!(h.replaces.is_empty());
    }

    #[test]
    fn xml_round_trip() {
        let mut h = FindHistory::default();
        h.push_find("alpha");
        h.push_find("beta");
        h.push_replace("BETA");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("find_history.xml");
        h.save(&path).expect("save");
        let back = FindHistory::load(&path).expect("load");
        assert_eq!(back, h);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.xml");
        let h = FindHistory::load(&path).expect("load");
        assert!(h.finds.is_empty());
        assert!(h.replaces.is_empty());
    }
}
