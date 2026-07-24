//! Persisted user preferences. The Settings → Preferences dialog
//! reads from this at open time and writes back on Close; the
//! Shell holds one live instance and hands out references to
//! whichever feature needs the tuning knobs.
//!
//! Backed by `config.xml` next to `session.xml`. Missing file is
//! the first-launch case (returns defaults); corrupt file is
//! logged + replaced with defaults so a hand-edit that breaks
//! the schema can never lock the user out of the app.
//!
//! **Scope note.** Phase 4 wiring populates only the sections
//! that back live UI: right now that is
//! [`RecentFilesHistoryConfig`]. Other Preferences panes land
//! one by one and each adds a new field with a `#[serde(default)]`
//! attribute so downgrading a config from a future version keeps
//! working.
//!
//! Schema:
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <preferences>
//!   <recent-files-history>
//!     <enabled>true</enabled>
//!     <max-entries>10</max-entries>
//!     <in-submenu>false</in-submenu>
//!     <display-mode>full-path</display-mode>
//!     <custom-max-length>60</custom-max-length>
//!   </recent-files-history>
//! </preferences>
//! ```

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Root of `config.xml`. Every future panel adds a
/// `#[serde(default)]` field here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "preferences")]
pub struct Preferences {
    /// Preferences → Recent Files History panel.
    #[serde(rename = "recent-files-history", default)]
    pub recent_files_history: RecentFilesHistoryConfig,
}

/// The Preferences → Recent Files History panel's persisted
/// controls. Field semantics mirror N++ (Code++ is a 1:1
/// clone) so the two apps agree pane-by-pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentFilesHistoryConfig {
    /// "Don't check at launch time" in N++'s pane, expressed
    /// here as the positive-sense `enabled` because the storage
    /// XML reads more naturally that way. When `false` the
    /// Shell never pushes to the recent-files list on close and
    /// the menu never shows the recent-files section.
    ///
    /// The redundancy with `max_entries == 0` is inherited from
    /// N++ — the two controls historically governed different
    /// things ("skip missing-file check on startup" vs "track
    /// nothing"). Kept independent so the two configurations
    /// round-trip losslessly across the N++ ↔ Code++ boundary.
    #[serde(default = "defaults::enabled")]
    pub enabled: bool,
    /// Hard cap on the retained recent-files list. Valid range
    /// 0..=30 (matches N++'s spinner); values outside are
    /// clamped on load. Matches N++ behaviour exactly:
    /// lowering `max_entries` from 20 to 5 shrinks the on-disk
    /// list to 5 (entries 6..20 are dropped, not just hidden),
    /// and raising it back to 20 does not resurrect them.
    /// `MAX_ENTRIES = 30` in [`crate::recent_files::MAX_ENTRIES`]
    /// is a defense ceiling the core enforces regardless of
    /// this preference — it caps hand-edited XML at read time.
    #[serde(default = "defaults::max_entries")]
    pub max_entries: u32,
    /// If `true`, the File menu wraps the recent-files region
    /// in a single "Recent Files" submenu — the file list, an
    /// inner separator, and the three action entries (Restore /
    /// Open All / Empty) all nest inside that popup. If `false`
    /// (the default), the region is inlined flat on the File
    /// menu itself.
    #[serde(default)]
    pub in_submenu: bool,
    /// Controls how each entry's label renders in the menu.
    #[serde(default = "defaults::display_mode")]
    pub display_mode: RecentFileDisplayMode,
    /// Only consulted when `display_mode == CustomMaxLength`.
    /// Truncates the full path to this many characters, then
    /// inserts a leading `...` to signal truncation. Valid
    /// range 1..=259 (matches N++'s spinner, whose 259 ceiling
    /// is `MAX_PATH - 1`); clamped on load.
    #[serde(default = "defaults::custom_max_length")]
    pub custom_max_length: u32,
}

impl Default for RecentFilesHistoryConfig {
    fn default() -> Self {
        Self {
            enabled: defaults::enabled(),
            max_entries: defaults::max_entries(),
            in_submenu: false,
            display_mode: defaults::display_mode(),
            custom_max_length: defaults::custom_max_length(),
        }
    }
}

impl RecentFilesHistoryConfig {
    /// Coerce out-of-range values into their nearest valid
    /// neighbour so the rest of the UI can trust these fields
    /// without re-validating. Called by both the loader (to
    /// tolerate hand-edits) and the Preferences dialog's Close
    /// path (to tolerate paste-into-spinner edge cases).
    pub fn clamp(&mut self) {
        if self.max_entries > MAX_ENTRIES_LIMIT {
            self.max_entries = MAX_ENTRIES_LIMIT;
        }
        if self.custom_max_length == 0 {
            self.custom_max_length = 1;
        } else if self.custom_max_length > CUSTOM_MAX_LENGTH_LIMIT {
            self.custom_max_length = CUSTOM_MAX_LENGTH_LIMIT;
        }
    }

    /// Single source of truth for "is the recent-files feature
    /// live?" — every push / pop / drain / render path funnels
    /// through this so a user-facing toggle change lands
    /// atomically everywhere. Two conditions must hold: the
    /// user hasn't cleared "Don't check at launch time" and the
    /// visible cap is above zero (a cap of 0 shows nothing, so
    /// tracking entries no one will see just wastes disk).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.max_entries > 0
    }

    /// The visible string for `path` under this config's [`display_mode`]
    /// — the part that follows a recent-files menu entry's number, before
    /// any platform mnemonic escaping or sanitisation. Shared by both UI
    /// backends so the three modes render identically:
    ///
    /// * [`RecentFileDisplayMode::OnlyFileName`] — the last path component.
    /// * [`RecentFileDisplayMode::FullPath`] — the path verbatim.
    /// * [`RecentFileDisplayMode::CustomMaxLength`] — the full path
    ///   truncated to [`custom_max_length`] characters, keeping the tail
    ///   (usually the filename) and prepending `...` when it doesn't fit.
    ///
    /// [`display_mode`]: Self::display_mode
    /// [`custom_max_length`]: Self::custom_max_length
    #[must_use]
    pub fn display_path(&self, path: &std::path::Path) -> String {
        match self.display_mode {
            RecentFileDisplayMode::OnlyFileName => path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |s| s.to_string_lossy().into_owned(),
            ),
            RecentFileDisplayMode::FullPath => path.to_string_lossy().into_owned(),
            RecentFileDisplayMode::CustomMaxLength => {
                let full = path.to_string_lossy();
                let cap = self.custom_max_length as usize;
                if full.chars().count() <= cap {
                    full.into_owned()
                } else {
                    // Keep the last `cap - 3` characters and prepend "..."
                    // so the meaningful tail stays visible. Char-aware so a
                    // multibyte sequence is never sliced mid-codepoint.
                    let keep = cap.saturating_sub(3);
                    let start = full.chars().count().saturating_sub(keep);
                    let tail: String = full.chars().skip(start).collect();
                    format!("...{tail}")
                }
            }
        }
    }
}

/// Upper bound on `RecentFilesHistoryConfig::max_entries`.
/// Matches N++'s spinner and [`crate::recent_files::MAX_ENTRIES`].
pub const MAX_ENTRIES_LIMIT: u32 = 30;

/// Upper bound on `RecentFilesHistoryConfig::custom_max_length`.
/// `MAX_PATH - 1` on Windows — the same value N++ hard-codes
/// into the spinner.
pub const CUSTOM_MAX_LENGTH_LIMIT: u32 = 259;

/// How to render each recent-files menu label. Mirrors the
/// three radio options in the N++ pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecentFileDisplayMode {
    /// Just the last path component.
    OnlyFileName,
    /// The full path verbatim (N++ default).
    #[default]
    FullPath,
    /// Full path, truncated to `custom_max_length` characters
    /// with a leading ellipsis when it doesn't fit.
    CustomMaxLength,
}

mod defaults {
    pub(super) fn enabled() -> bool {
        true
    }
    pub(super) fn max_entries() -> u32 {
        10
    }
    pub(super) fn display_mode() -> super::RecentFileDisplayMode {
        super::RecentFileDisplayMode::FullPath
    }
    pub(super) fn custom_max_length() -> u32 {
        60
    }
}

/// Errors from reading or writing `config.xml`. Mirrors the
/// shape of [`crate::find_history::FindHistoryError`].
#[derive(Debug)]
pub enum PreferencesError {
    /// I/O error reading or writing the file.
    Io(std::io::Error),
    /// The file existed but didn't parse — corrupt or wrong
    /// schema. Callers log + start from `Preferences::default`.
    Parse(quick_xml::DeError),
    /// Serialization failure. Effectively impossible.
    Serialize(quick_xml::SeError),
}

impl std::fmt::Display for PreferencesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreferencesError::Io(e) => write!(f, "config.xml I/O: {e}"),
            PreferencesError::Parse(e) => write!(f, "config.xml parse: {e}"),
            PreferencesError::Serialize(e) => write!(f, "config.xml serialize: {e}"),
        }
    }
}

impl std::error::Error for PreferencesError {}

impl Preferences {
    /// Read `config.xml` at `path`. Missing file → default
    /// preferences (first-launch case). Every loaded pane is
    /// clamped so downstream code can trust the ranges.
    ///
    /// # Errors
    ///
    /// Returns [`PreferencesError::Io`] for any filesystem
    /// failure other than "not found", and
    /// [`PreferencesError::Parse`] when the file exists but
    /// doesn't deserialise.
    pub fn load(path: &Path) -> Result<Self, PreferencesError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(PreferencesError::Io(e)),
        };
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut me: Self = quick_xml::de::from_str(&raw).map_err(PreferencesError::Parse)?;
        me.recent_files_history.clamp();
        Ok(me)
    }

    /// Atomic write to `path`, same shape used by
    /// [`crate::find_history::FindHistory::save`] and
    /// [`crate::recent_files::RecentFiles::save`].
    ///
    /// # Errors
    ///
    /// Returns [`PreferencesError::Serialize`] for a quick-xml
    /// emit failure (effectively impossible with these types),
    /// and [`PreferencesError::Io`] for any filesystem failure
    /// during the create-dirs / tempfile / write / sync / rename
    /// pipeline.
    pub fn save(&self, path: &Path) -> Result<(), PreferencesError> {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        quick_xml::se::to_writer(&mut xml, self).map_err(PreferencesError::Serialize)?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent).map_err(PreferencesError::Io)?;
        }
        let parent_dir = parent.unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".preferences-")
            .suffix(".xml.tmp")
            .tempfile_in(parent_dir)
            .map_err(PreferencesError::Io)?;
        tmp.write_all(xml.as_bytes())
            .map_err(PreferencesError::Io)?;
        tmp.as_file_mut().sync_all().map_err(PreferencesError::Io)?;
        tmp.persist(path)
            .map_err(|e| PreferencesError::Io(e.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_notepadpp_first_launch() {
        let p = Preferences::default();
        assert!(p.recent_files_history.enabled);
        assert_eq!(p.recent_files_history.max_entries, 10);
        assert!(!p.recent_files_history.in_submenu);
        assert_eq!(
            p.recent_files_history.display_mode,
            RecentFileDisplayMode::FullPath
        );
        assert_eq!(p.recent_files_history.custom_max_length, 60);
    }

    #[test]
    fn display_path_only_file_name_keeps_last_component() {
        let cfg = RecentFilesHistoryConfig {
            display_mode: RecentFileDisplayMode::OnlyFileName,
            ..Default::default()
        };
        assert_eq!(
            cfg.display_path(std::path::Path::new("/home/max/notes/todo.md")),
            "todo.md"
        );
    }

    #[test]
    fn display_path_full_path_is_verbatim() {
        let cfg = RecentFilesHistoryConfig {
            display_mode: RecentFileDisplayMode::FullPath,
            ..Default::default()
        };
        assert_eq!(
            cfg.display_path(std::path::Path::new("/a/b/c.txt")),
            "/a/b/c.txt"
        );
    }

    #[test]
    fn display_path_custom_length_truncates_keeping_the_tail() {
        let cfg = RecentFilesHistoryConfig {
            display_mode: RecentFileDisplayMode::CustomMaxLength,
            custom_max_length: 12,
            ..Default::default()
        };
        // 19-char path, cap 12 → "..." + the last 9 chars ("/path.txt").
        assert_eq!(
            cfg.display_path(std::path::Path::new("/very/long/path.txt")),
            ".../path.txt"
        );
        // A path within the cap is returned verbatim (no ellipsis).
        assert_eq!(
            cfg.display_path(std::path::Path::new("/a/b.txt")),
            "/a/b.txt"
        );
    }

    #[test]
    fn clamp_pushes_out_of_range_values_into_bounds() {
        let mut c = RecentFilesHistoryConfig {
            max_entries: 999,
            custom_max_length: 999,
            ..Default::default()
        };
        c.clamp();
        assert_eq!(c.max_entries, MAX_ENTRIES_LIMIT);
        assert_eq!(c.custom_max_length, CUSTOM_MAX_LENGTH_LIMIT);
    }

    #[test]
    fn clamp_zero_custom_length_becomes_one() {
        let mut c = RecentFilesHistoryConfig {
            custom_max_length: 0,
            ..Default::default()
        };
        c.clamp();
        assert_eq!(c.custom_max_length, 1);
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.xml");
        let p = Preferences {
            recent_files_history: RecentFilesHistoryConfig {
                enabled: false,
                max_entries: 25,
                in_submenu: true,
                display_mode: RecentFileDisplayMode::CustomMaxLength,
                custom_max_length: 120,
            },
        };
        p.save(&path).expect("save");
        let back = Preferences::load(&path).expect("load");
        assert_eq!(back, p);
    }

    #[test]
    fn missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope.xml");
        assert_eq!(
            Preferences::load(&path).expect("load"),
            Preferences::default()
        );
    }

    #[test]
    fn load_clamps_out_of_range_hand_edits() {
        // Round-trip through save to get the exact shape the
        // deserializer accepts (quick-xml is whitespace-sensitive
        // at container boundaries), then verify the clamp path on
        // the two out-of-range values.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.xml");
        let bad = Preferences {
            recent_files_history: RecentFilesHistoryConfig {
                max_entries: 500,
                custom_max_length: 0,
                ..Default::default()
            },
        };
        bad.save(&path).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read");
        // quick-xml chooses per-field layout (some elements as
        // attributes, some as children) so just verify the two
        // out-of-range digits made it into the file — the load
        // → clamp path is what actually matters.
        assert!(raw.contains("500"));
        let p = Preferences::load(&path).expect("load");
        assert_eq!(p.recent_files_history.max_entries, MAX_ENTRIES_LIMIT);
        assert_eq!(p.recent_files_history.custom_max_length, 1);
    }
}
