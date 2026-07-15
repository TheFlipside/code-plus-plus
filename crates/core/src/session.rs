//! Session model and `session.xml` round-trip.
//!
//! On clean shutdown the editor writes the open tabs and per-tab cursor
//! position to a single XML file under the user's config directory; on
//! the next start it reads that file back and reconstructs the tab
//! list. See DESIGN.md §7.2 Phase 2.
//!
//! The XML schema is Code++-native — we deliberately do **not** match
//! Notepad++'s `session.xml` shape, because the binary plugin ABI is
//! the only Notepad++ compatibility surface (DESIGN.md §6.1). Internal
//! state files like the session index are ours to design.
//!
//! Schema (stable from Phase 2 onward):
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <session active="0">
//!   <window width="1280" height="720" maximized="false"/>
//!   <tab path="C:\path\to\file.txt" cursor="42"
//!        encoding="UTF-8" eol="LF"/>
//! </session>
//! ```
//!
//! The `<window>` element is optional. A session.xml written before
//! the window-geometry feature shipped, or by a future build that
//! drops it, parses cleanly with `window: None` and the UI falls
//! back to its built-in default size. Width/height are pixel
//! dimensions of the *restored* (non-maximized) outer window —
//! storing the restored geometry alongside the maximized flag is
//! what lets the next launch start maximized but still know the
//! "small" size to fall back to when the user un-maximizes.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::encoding::Encoding;
use crate::eol::Eol;

/// One open tab's persistent state.
///
/// Two flavours:
///
/// 1. **Saved file**: `path` is `Some(...)`, `untitled_seq` is `None`,
///    `backup` is normally `None`. The on-disk file is the source of
///    truth; restore re-reads it. (A future iteration may also write
///    a backup for *dirty* saved files so unsaved edits survive the
///    next launch — not yet implemented.)
/// 2. **Untitled buffer ("new N")**: `path` is `None`,
///    `untitled_seq` is `Some(N)`, `backup` is `Some(filename)`. The
///    backup file under `platform::backups_dir()` carries the buffer's
///    text content. Restore re-creates the tab as untitled and seeds
///    its Scintilla document from the backup file. This is the
///    Notepad++-style "unsaved work always survives a restart"
///    behaviour the user relies on indefinitely.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    /// Full path to the file this tab represents. `Some` for saved
    /// files; `None` for untitled buffers (which are tracked via
    /// `untitled_seq` instead).
    #[serde(rename = "@path", skip_serializing_if = "Option::is_none", default)]
    pub path: Option<PathBuf>,
    /// Caret byte position within the buffer. Restored on load via
    /// `SCI_GOTOPOS`. Defaults to 0 for files we've never opened.
    #[serde(rename = "@cursor", default)]
    pub cursor: u64,
    /// Detected encoding when the tab was last opened. Persisted so a
    /// session restore picks up where the user left off without redoing
    /// detection on every file.
    #[serde(rename = "@encoding", default)]
    pub encoding: Encoding,
    /// EOL style. Same rationale as `encoding`.
    #[serde(rename = "@eol", default)]
    pub eol: Eol,
    /// `Some(N)` for unsaved "new N" buffers; `None` for saved
    /// files. The number is round-tripped verbatim so a user who
    /// closes with `new 3` open and `new 1`/`new 2` already saved
    /// reopens with `new 3` rather than the system reassigning
    /// numbers.
    #[serde(
        rename = "@untitled_seq",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub untitled_seq: Option<u32>,
    /// Filename (relative to `platform::backups_dir()`) of the backup
    /// file that holds this tab's text content. `Some` for any tab
    /// whose content can't be reproduced from disk on next launch
    /// (currently every untitled buffer). The backup file is the
    /// raw UTF-8 text of the buffer; the `encoding` and `eol` fields
    /// describe the *target* encoding the user wants applied when
    /// they eventually save to a real path.
    #[serde(rename = "@backup", skip_serializing_if = "Option::is_none", default)]
    pub backup: Option<String>,
    /// User-chosen display name for an untitled buffer, set via
    /// File → Rename... and persisted so a renamed "new 3"
    /// (relabelled e.g. "release notes") comes back with that
    /// label rather than reverting to its sequence number after a
    /// restart. Always `None` for path-bound buffers — those
    /// derive their display name from `path` and the rename UI
    /// routes them to Save-As instead. Older session.xml files
    /// (written before this field shipped) round-trip cleanly
    /// thanks to `default`.
    #[serde(
        rename = "@custom_name",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub custom_name: Option<String>,
    /// N++-compatible `LangType` id for this buffer. Persisted as
    /// the raw `i32` so the wire format matches what plugins see
    /// over `NPPM_GETBUFFERLANGTYPE` — including unknown future
    /// values that Code++'s `LANG_TABLE` doesn't recognise yet.
    /// `None` means "no stored override" — the load path falls
    /// back to extension-based auto-detection for path-bound tabs
    /// or `L_TEXT` for untitled. On save the value comes from
    /// `shell::Tab.lang`, which the Language menu writes when the
    /// user picks a different language; the next restore brings
    /// the choice back so manual overrides survive relaunches.
    /// Older session.xml files round-trip cleanly thanks to
    /// `default`.
    #[serde(rename = "@lang", skip_serializing_if = "Option::is_none", default)]
    pub lang: Option<i32>,
    /// `true` iff the user pinned this tab. Pinned tabs stay
    /// clustered at the left edge of the tab strip in insertion
    /// order and cannot be moved by drag; unpinned tabs occupy the
    /// slots to their right and remain freely draggable. The Shell
    /// enforces the "pinned-before-unpinned" invariant at every
    /// mutation. Older session.xml files (written before this
    /// field shipped) round-trip cleanly thanks to `default` —
    /// tabs come back unpinned, matching pre-pin behaviour.
    #[serde(rename = "@pinned", default, skip_serializing_if = "is_false")]
    pub pinned: bool,
}

/// Persisted main-window geometry. Pixel dimensions are positive in
/// practice; signed `i32` matches the Win32 / GTK / Cocoa native
/// types so the UI can pass values straight through without
/// arithmetic on unsigned widths producing surprising results when
/// the OS reports negative work-area coordinates on multi-monitor
/// setups (a left-of-primary monitor has negative `x`).
///
/// Position (`x`, `y`) is intentionally not stored in this initial
/// cut — the user's request was about size. Adding position later
/// is purely additive (new `Option<i32>` fields default to `None`
/// and existing session.xml files round-trip unchanged).
///
/// `Default` produces `{ width: None, height: None, maximized: false }`
/// — which is *load-bearing*: the UI's runtime tracking calls
/// `Shell::saved_window_geometry().unwrap_or_default()` on every
/// `WM_SIZE`, so flipping any field to a non-zero default would
/// silently rewrite the saved state on every interaction. Keep
/// the all-zero default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    /// Restored (non-maximized) outer width in pixels. The UI is
    /// expected to clamp against the actual screen size and any
    /// minimum-width floor (e.g. "wide enough to show every
    /// toolbar button") before applying.
    #[serde(rename = "@width", skip_serializing_if = "Option::is_none", default)]
    pub width: Option<i32>,
    /// Restored outer height in pixels. Same UI-side clamp
    /// expectation as `width`.
    #[serde(rename = "@height", skip_serializing_if = "Option::is_none", default)]
    pub height: Option<i32>,
    /// True iff the window was maximized at the moment session.xml
    /// was last written. The UI restores this by showing
    /// maximized while still using the `width`/`height` as the
    /// "un-maximize back to this size" fallback.
    #[serde(rename = "@maximized", default, skip_serializing_if = "is_false")]
    pub maximized: bool,
}

/// `skip_serializing_if` predicate for the maximized flag — so the
/// common `maximized="false"` case isn't serialized at all,
/// matching how the other `Option` fields elide their default.
///
/// serde calls predicates via `&T`, so the signature must be
/// `&bool`. The clippy `trivially_copy_pass_by_ref` lint would
/// prefer `bool`-by-value for a 1-byte type; the by-ref shape
/// is mandated by serde here, not the API's choice.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Persisted "Folder as Workspace" panel state — the left-column
/// tree view reachable via `File → Open Folder as Workspace...`.
/// Only present on sessions that had the panel opened at least
/// once; a session.xml written before the feature shipped or by
/// a build that drops it deserialises to `None`, which the UI
/// reads as "no workspace to restore, start fresh."
///
/// The `root` field uses serde's blanket `Option<PathBuf>`
/// support — same mechanism `Tab.path` uses. Kept as `Option`
/// so the "panel was opened but never rooted at a real folder"
/// state (impossible today, but not ruled out by the ABI)
/// serialises as no `root` attribute rather than `root=""`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSession {
    /// Absolute path of the folder the panel was rooted at when
    /// this session was saved. `None` if the user closed the
    /// panel without opening a folder this session; in practice
    /// the UI only writes this field when a root has been
    /// picked, so `None` after a load means "no restore."
    #[serde(rename = "@root", skip_serializing_if = "Option::is_none", default)]
    pub root: Option<PathBuf>,
    /// True iff the panel was visible when the session was
    /// saved. Cold-start restore only pops the panel open when
    /// this is true — a user who closed the panel expects it to
    /// stay closed next launch even if `root` is remembered.
    #[serde(rename = "@visible", default, skip_serializing_if = "is_false")]
    pub visible: bool,
    /// Panel width in pixels when the session was saved. `None`
    /// means "use the built-in default" — same fallback shape
    /// as [`WindowGeometry::width`].
    #[serde(rename = "@width", skip_serializing_if = "Option::is_none", default)]
    pub width: Option<i32>,
}

/// Persisted global (view-level) editor toggles. Room to grow — the
/// initial cut carries just the indent-guide flag, but the intent is
/// that other cross-buffer view state (word-wrap default, show-line-
/// numbers default, show-whitespace default, …) lands on the same
/// element so a session.xml written by an older build round-trips
/// cleanly against a newer one and vice versa.
///
/// `Default` produces the "everything off" baseline, which matches
/// Scintilla's built-in defaults so a session with no `<view>`
/// element (first launch, or a `session.xml` written before this
/// feature shipped) behaves identically to a session with an empty
/// `<view/>` element.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSettings {
    /// True iff the indent-guide mode is on (Scintilla's
    /// `SC_IV_LOOKBOTH`). False maps to `SC_IV_NONE`. Toggled by
    /// the toolbar's `ID_VIEW_SHOW_INDENT_GUIDE` and persisted on
    /// the next session save so the next launch reopens with the
    /// user's chosen state instead of Scintilla's default (off).
    #[serde(rename = "@indent_guide", default, skip_serializing_if = "is_false")]
    pub indent_guide: bool,
}

/// The whole session. The active-tab index is `Option<usize>` rather
/// than `usize` so an empty session round-trips cleanly (no spurious
/// `active="0"` when there are no tabs).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "session")]
pub struct Session {
    /// Index into `tabs` of the currently focused tab, or `None` if
    /// no tabs are open.
    #[serde(rename = "@active", skip_serializing_if = "Option::is_none", default)]
    pub active: Option<usize>,
    /// Persisted main-window geometry. `None` on a session.xml
    /// written before this feature shipped (or by a build that
    /// drops it) — UI falls back to its built-in default size.
    #[serde(rename = "window", skip_serializing_if = "Option::is_none", default)]
    pub window: Option<WindowGeometry>,
    /// Persisted "Folder as Workspace" panel state. `None` on
    /// sessions written before the feature shipped or by a build
    /// that drops it — cold-start restore treats that as "no
    /// panel this session."
    #[serde(rename = "workspace", skip_serializing_if = "Option::is_none", default)]
    pub workspace: Option<WorkspaceSession>,
    /// Persisted global editor view toggles (indent guide, and
    /// future siblings). Round-trips as a `<view/>` element via
    /// [`ViewSettings`]; absence deserialises to the all-off
    /// default.
    #[serde(rename = "view", default, skip_serializing_if = "is_default_view")]
    pub view: ViewSettings,
    /// All open tabs, in the order they appear in the tab strip.
    #[serde(rename = "tab", default)]
    pub tabs: Vec<Tab>,
}

/// `skip_serializing_if` predicate for [`Session::view`] — elides the
/// `<view/>` element from serialised session.xml when every toggle
/// carries its default value (there's nothing to write). Same
/// discipline the `Option`-typed sibling fields use for their
/// "absent" case.
///
/// serde calls predicates via `&T`, so the signature must be
/// `&ViewSettings` even though the type is `Copy` — same reason the
/// sibling `is_false` predicate above pins the `&bool` shape.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_view(v: &ViewSettings) -> bool {
    *v == ViewSettings::default()
}

/// Errors from reading or writing `session.xml`.
#[derive(Debug)]
pub enum SessionError {
    /// I/O error reading or writing the file.
    Io(std::io::Error),
    /// The file existed but wasn't valid XML, or didn't match the
    /// expected schema.
    Parse(quick_xml::DeError),
    /// Serialization to XML failed (extremely rare — usually only on
    /// non-UTF-8 content in attributes, which we don't produce).
    Serialize(quick_xml::SeError),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(e) => write!(f, "session I/O error: {e}"),
            SessionError::Parse(e) => write!(f, "session parse error: {e}"),
            SessionError::Serialize(e) => write!(f, "session serialize error: {e}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(e)
    }
}

impl Session {
    /// Convenience constructor for an empty session.
    #[must_use]
    pub fn new() -> Self {
        Session::default()
    }

    /// Read `session.xml` from `path`. A missing file is **not** an
    /// error — it returns an empty `Session`, matching the "first
    /// launch, nothing to restore" UX.
    ///
    /// # Errors
    ///
    /// Returns `SessionError::Io` on any read failure other than
    /// "not found"; returns `SessionError::Parse` when the bytes
    /// are present but don't deserialise into the
    /// `Session`/`Tab` shape (e.g. corrupt or hand-edited
    /// session.xml).
    pub fn load_from_xml(path: &Path) -> Result<Self, SessionError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Session::default());
            }
            Err(e) => return Err(SessionError::Io(e)),
        };
        if contents.trim().is_empty() {
            return Ok(Session::default());
        }
        quick_xml::de::from_str(&contents).map_err(SessionError::Parse)
    }

    /// Write `session.xml` atomically.
    ///
    /// Uses `tempfile::NamedTempFile` to create a sibling temp file
    /// with restrictive permissions (owner-only on Unix; a private
    /// handle on Windows), writes and `sync_all`s it to disk, then
    /// `persist`s with an atomic rename. Three guarantees follow:
    ///
    ///   - **Power-loss safety:** `sync_all` flushes the write to
    ///     stable storage before the rename, so a crash mid-save
    ///     leaves either the old session.xml intact or the new one
    ///     fully written, never a torn file.
    ///   - **No stale tmp files:** if any step fails, dropping the
    ///     `NamedTempFile` removes it. Earlier hand-rolled `.tmp`
    ///     siblings would accumulate forever on a failed rename.
    ///   - **No TOCTOU substitution:** a local attacker can't replace
    ///     the temp file between write and rename — the file has
    ///     restrictive permissions and a randomized name.
    ///
    /// # Errors
    ///
    /// Returns `SessionError::Serialize` if quick-xml fails to
    /// emit the document; `SessionError::Io` on parent-directory
    /// creation, temp-file creation/write/sync, or the final
    /// atomic rename failing.
    pub fn save_to_xml(&self, path: &Path) -> Result<(), SessionError> {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        quick_xml::se::to_writer(&mut xml, self).map_err(SessionError::Serialize)?;

        // Ensure the parent directory exists (first-run case where
        // %APPDATA%\Code++ has not been created yet). The
        // tempfile is anchored to this directory so persist() is a
        // same-filesystem rename and therefore atomic.
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent)?;
        }
        let parent_dir = parent.unwrap_or_else(|| Path::new("."));

        let mut tmp = tempfile::Builder::new()
            .prefix(".session-")
            .suffix(".xml.tmp")
            .tempfile_in(parent_dir)?;
        tmp.write_all(xml.as_bytes())?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(path).map_err(|e| SessionError::Io(e.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_session_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.xml");
        (dir, path)
    }

    #[test]
    fn load_missing_file_returns_empty_session() {
        let (_dir, path) = temp_session_path();
        // File doesn't exist yet.
        let session = Session::load_from_xml(&path).unwrap();
        assert!(session.tabs.is_empty());
        assert_eq!(session.active, None);
    }

    #[test]
    fn round_trip_empty_session() {
        let (_dir, path) = temp_session_path();
        let session = Session::default();
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
    }

    #[test]
    fn round_trip_session_with_tabs() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: Some(1),
            window: None,
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![
                Tab {
                    path: Some(PathBuf::from(r"C:\users\alice\hello.txt")),
                    cursor: 0,
                    encoding: Encoding::Utf8,
                    eol: Eol::Lf,
                    untitled_seq: None,
                    backup: None,
                    custom_name: None,
                    lang: None,
                    pinned: false,
                },
                Tab {
                    path: Some(PathBuf::from(r"C:\users\alice\config.toml")),
                    cursor: 142,
                    encoding: Encoding::Utf8Bom,
                    eol: Eol::CrLf,
                    untitled_seq: None,
                    backup: None,
                    custom_name: None,
                    lang: None,
                    pinned: false,
                },
            ],
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
    }

    #[test]
    fn round_trip_preserves_other_encoding() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: Some(0),
            window: None,
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![Tab {
                path: Some(PathBuf::from("legacy.txt")),
                cursor: 0,
                encoding: Encoding::Other("windows-1252".into()),
                eol: Eol::CrLf,
                untitled_seq: None,
                backup: None,
                custom_name: None,
                lang: None,
                pinned: false,
            }],
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
    }

    /// Untitled buffers carry `path: None`, an `untitled_seq`, and a
    /// `backup` filename. Round-trip exercises the new schema fields
    /// in isolation from saved-file tabs.
    #[test]
    fn round_trip_untitled_with_backup() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: Some(0),
            window: None,
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![Tab {
                path: None,
                cursor: 17,
                encoding: Encoding::Utf8,
                eol: Eol::Lf,
                untitled_seq: Some(1),
                backup: Some("new 1@2026-05-04_215750".into()),
                custom_name: None,
                lang: None,
                pinned: false,
            }],
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
    }

    /// Mixed session: a saved file, two untitled buffers, and a saved
    /// active index that points at one of the untitled tabs. The
    /// active-index → list-position mapping must round-trip
    /// regardless of where the untitled tabs sit in the list.
    #[test]
    fn round_trip_mixed_saved_and_untitled() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: Some(1),
            window: None,
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![
                Tab {
                    path: Some(PathBuf::from("/tmp/a.txt")),
                    cursor: 0,
                    encoding: Encoding::Utf8,
                    eol: Eol::Lf,
                    untitled_seq: None,
                    backup: None,
                    custom_name: None,
                    lang: None,
                    pinned: false,
                },
                Tab {
                    path: None,
                    cursor: 0,
                    encoding: Encoding::Utf8,
                    eol: Eol::Lf,
                    untitled_seq: Some(1),
                    backup: Some("new 1@2026-05-04_215800".into()),
                    custom_name: None,
                    lang: None,
                    pinned: false,
                },
                Tab {
                    path: None,
                    cursor: 0,
                    encoding: Encoding::Utf8,
                    eol: Eol::Lf,
                    untitled_seq: Some(2),
                    backup: Some("new 2@2026-05-04_215800".into()),
                    custom_name: None,
                    lang: None,
                    pinned: false,
                },
            ],
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
    }

    /// `custom_name` round-trips on an untitled buffer. Verifies the
    /// File → Rename... label survives a session save/load cycle so
    /// a user-renamed `new 1` (relabelled e.g. "release notes")
    /// comes back with the chosen name rather than reverting to the
    /// sequence number.
    #[test]
    fn round_trip_untitled_with_custom_name() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: Some(0),
            window: None,
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![Tab {
                path: None,
                cursor: 0,
                encoding: Encoding::Utf8,
                eol: Eol::Lf,
                untitled_seq: Some(3),
                backup: Some("new 3@2026-05-09_141500".into()),
                custom_name: Some("release notes".into()),
                lang: None,
                pinned: false,
            }],
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
        assert_eq!(loaded.tabs[0].custom_name.as_deref(), Some("release notes"));
    }

    /// `lang` round-trips on a saved-file tab. Verifies a user's
    /// Language-menu override (`tab.lang` written by
    /// `NPPM_SETBUFFERLANGTYPE`) survives the session save/load
    /// cycle so the manual choice persists across relaunches
    /// instead of reverting to extension-based auto-detection.
    #[test]
    fn round_trip_tab_with_manual_lang() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: Some(0),
            window: None,
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![Tab {
                path: Some(PathBuf::from("notes.txt")),
                cursor: 0,
                encoding: Encoding::Utf8,
                eol: Eol::Lf,
                untitled_seq: None,
                backup: None,
                custom_name: None,
                // 81 is L_RUST. The point of the test is that the
                // user manually picked Rust for a `.txt` file —
                // extension-based detection would yield Text, so
                // the persisted override must dominate on restore.
                lang: Some(81),
                pinned: false,
            }],
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
        assert_eq!(loaded.tabs[0].lang, Some(81));
    }

    /// `pinned` round-trips on a saved-file tab. Confirms the
    /// user's pin choice persists across the session save/load
    /// cycle so a pinned tab comes back pinned on next launch.
    #[test]
    fn round_trip_tab_with_pinned_true() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: Some(0),
            window: None,
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![Tab {
                path: Some(PathBuf::from("pinned.txt")),
                cursor: 42,
                encoding: Encoding::Utf8,
                eol: Eol::Lf,
                untitled_seq: None,
                backup: None,
                custom_name: None,
                lang: None,
                pinned: true,
            }],
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
        assert!(loaded.tabs[0].pinned, "pinned=true must round-trip");
    }

    /// The `@pinned="false"` default is elided from the wire —
    /// `skip_serializing_if = "is_false"` should keep the XML
    /// clean the same way `@maximized` does. Loading the same
    /// bytes must still yield `pinned: false`.
    #[test]
    fn round_trip_pinned_false_is_omitted_from_xml() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: Some(0),
            window: None,
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![Tab {
                path: Some(PathBuf::from("plain.txt")),
                cursor: 0,
                encoding: Encoding::Utf8,
                eol: Eol::Lf,
                untitled_seq: None,
                backup: None,
                custom_name: None,
                lang: None,
                pinned: false,
            }],
        };
        session.save_to_xml(&path).unwrap();
        let xml = std::fs::read_to_string(&path).unwrap();
        assert!(
            !xml.contains("pinned"),
            "pinned=false must be elided from the wire; got:\n{xml}"
        );
        let loaded = Session::load_from_xml(&path).unwrap();
        assert!(!loaded.tabs[0].pinned);
    }

    /// A session.xml written before the `@pinned` attribute
    /// shipped must still parse — old files come back with
    /// `pinned: false` and untouched by every other feature.
    #[test]
    fn pre_pinned_session_xml_loads_with_none_pinned() {
        let (_dir, path) = temp_session_path();
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<session active="0"><tab path="hello.txt" cursor="0" encoding="UTF-8" eol="LF"/></session>"#;
        std::fs::write(&path, xml).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(loaded.tabs.len(), 1);
        assert!(
            !loaded.tabs[0].pinned,
            "missing @pinned must default to false"
        );
    }

    /// A session.xml written before the `@lang` attribute shipped
    /// must still parse — the load path falls back to extension
    /// detection (or `L_TEXT` for untitled) when `lang` is `None`.
    #[test]
    fn pre_lang_session_xml_loads_with_none_lang() {
        let (_dir, path) = temp_session_path();
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<session active="0"><tab path="hello.txt" cursor="0" encoding="UTF-8" eol="LF"/></session>"#;
        std::fs::write(&path, xml).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].lang, None);
    }

    /// A session.xml written before the untitled-buffer feature
    /// shipped only carries `<tab path="..."/>` entries. Confirm
    /// they parse cleanly with `untitled_seq` and `backup`
    /// defaulting to `None`.
    #[test]
    fn pre_untitled_session_xml_loads_with_none_fields() {
        let (_dir, path) = temp_session_path();
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<session active="0"><tab path="hello.txt" cursor="0" encoding="UTF-8" eol="LF"/></session>"#;
        std::fs::write(&path, xml).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(
            loaded.tabs[0].path.as_deref(),
            Some(std::path::Path::new("hello.txt"))
        );
        assert_eq!(loaded.tabs[0].untitled_seq, None);
        assert_eq!(loaded.tabs[0].backup, None);
    }

    #[test]
    fn round_trip_window_geometry() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: None,
            window: Some(WindowGeometry {
                width: Some(1440),
                height: Some(900),
                maximized: false,
            }),
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![],
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
    }

    #[test]
    fn round_trip_window_geometry_maximized() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: None,
            window: Some(WindowGeometry {
                width: Some(1280),
                height: Some(720),
                maximized: true,
            }),
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![],
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
    }

    /// A session.xml written before the window-geometry feature
    /// shipped must still parse — the UI is expected to fall back
    /// to its built-in default size when `window` is `None`.
    #[test]
    fn pre_window_session_xml_loads_without_geometry() {
        let (_dir, path) = temp_session_path();
        // Verbatim shape of the old schema (no <window> element).
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<session active="0"><tab path="hello.txt" cursor="0" encoding="UTF-8" eol="LF"/></session>"#;
        std::fs::write(&path, xml).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(loaded.active, Some(0));
        assert_eq!(loaded.window, None);
        assert_eq!(loaded.tabs.len(), 1);
    }

    /// Default `WindowGeometry` (all `None` / `false`) shouldn't
    /// emit any `<window>` element — `skip_serializing_if` on the
    /// outer `Session.window` field handles that, but only when
    /// the field is `None`. Confirm the elision so a future change
    /// that swaps the field type is caught.
    #[test]
    fn empty_window_geometry_not_serialized() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: None,
            window: None,
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![],
        };
        session.save_to_xml(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("<window"),
            "<window> element should be elided when None: {text}"
        );
    }

    /// Round-trip a session with a populated `WorkspaceSession` —
    /// root, visible, and width all preserved.
    #[test]
    fn round_trip_workspace_session() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: None,
            window: None,
            workspace: Some(WorkspaceSession {
                root: Some(PathBuf::from(r"C:\Users\Max\Projects\code-plus-plus")),
                visible: true,
                width: Some(280),
            }),
            view: ViewSettings::default(),
            tabs: vec![],
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
        let ws = loaded.workspace.expect("workspace should round-trip");
        assert_eq!(
            ws.root.as_deref(),
            Some(std::path::Path::new(
                r"C:\Users\Max\Projects\code-plus-plus"
            ))
        );
        assert!(ws.visible);
        assert_eq!(ws.width, Some(280));
    }

    /// Workspace `visible=false` is the default state — it must
    /// elide from the XML the same way `@maximized="false"` does
    /// on `WindowGeometry`. Load must still recover it as `false`.
    #[test]
    fn workspace_visible_false_is_elided() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: None,
            window: None,
            workspace: Some(WorkspaceSession {
                root: Some(PathBuf::from(r"C:\code")),
                visible: false,
                width: None,
            }),
            view: ViewSettings::default(),
            tabs: vec![],
        };
        session.save_to_xml(&path).unwrap();
        let xml = std::fs::read_to_string(&path).unwrap();
        assert!(
            !xml.contains("visible"),
            "visible=false must be elided from the wire; got:\n{xml}"
        );
        let loaded = Session::load_from_xml(&path).unwrap();
        assert!(!loaded.workspace.unwrap().visible);
    }

    /// A `WorkspaceSession` with no root — pathological but not
    /// impossible — must serialise as `<workspace/>` (or
    /// `<workspace></workspace>`) with no `root` attribute, and
    /// load back as `WorkspaceSession { root: None, .. }`.
    #[test]
    fn workspace_root_none_elides_attribute() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: None,
            window: None,
            workspace: Some(WorkspaceSession {
                root: None,
                visible: false,
                width: None,
            }),
            view: ViewSettings::default(),
            tabs: vec![],
        };
        session.save_to_xml(&path).unwrap();
        let xml = std::fs::read_to_string(&path).unwrap();
        assert!(
            !xml.contains(" root="),
            "root=None must be elided from the wire; got:\n{xml}"
        );
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(loaded.workspace.unwrap().root, None);
    }

    /// A session.xml written before the `<workspace>` feature
    /// shipped must still parse. Cold-start restore treats a
    /// `None` workspace as "no panel this session" — no runtime
    /// error, no behavioural change relative to pre-feature.
    #[test]
    fn pre_workspace_session_xml_loads_with_none_workspace() {
        let (_dir, path) = temp_session_path();
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<session active="0"><tab path="hello.txt" cursor="0" encoding="UTF-8" eol="LF"/></session>"#;
        std::fs::write(&path, xml).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(loaded.workspace, None);
        // Tabs still parse cleanly — the added field doesn't
        // disturb any pre-existing round-trip.
        assert_eq!(loaded.tabs.len(), 1);
    }

    /// A session with no workspace (`None` field) must elide the
    /// `<workspace>` element entirely, same discipline as the
    /// `<window>` element.
    #[test]
    fn empty_workspace_not_serialized() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            active: None,
            window: None,
            workspace: None,
            view: ViewSettings::default(),
            tabs: vec![],
        };
        session.save_to_xml(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("workspace"),
            "<workspace> element should be elided when None: {text}"
        );
    }

    #[test]
    fn save_creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Two layers of not-yet-existing directories below the tempdir.
        let path = dir.path().join("config").join("codepp").join("session.xml");
        let session = Session::default();
        session.save_to_xml(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn malformed_xml_returns_parse_error() {
        let (_dir, path) = temp_session_path();
        std::fs::write(&path, b"<not-a-session-file>").unwrap();
        let err = Session::load_from_xml(&path).unwrap_err();
        assert!(matches!(err, SessionError::Parse(_)));
    }

    #[test]
    fn empty_file_treated_as_empty_session() {
        let (_dir, path) = temp_session_path();
        std::fs::write(&path, b"").unwrap();
        let session = Session::load_from_xml(&path).unwrap();
        assert!(session.tabs.is_empty());
    }

    #[test]
    fn atomic_write_does_not_leave_tmp_file() {
        let (dir, path) = temp_session_path();
        let session = Session::default();
        session.save_to_xml(&path).unwrap();

        // After a successful save, the only file in the directory must
        // be `session.xml` itself. tempfile uses a randomized name like
        // `.session-abcdef.xml.tmp`; persist() should have renamed it,
        // not left it behind.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries.len(), 1, "stray files left in dir: {entries:?}");
        assert_eq!(entries[0], "session.xml");
    }

    #[test]
    fn session_xml_starts_with_declaration() {
        let (_dir, path) = temp_session_path();
        Session::default().save_to_xml(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("<?xml"));
    }

    /// Round-trip a session with `view.indent_guide` set — the flag
    /// survives XML serialisation and reparses to the same value.
    #[test]
    fn round_trip_view_indent_guide_on() {
        let (_dir, path) = temp_session_path();
        let session = Session {
            view: ViewSettings { indent_guide: true },
            ..Session::default()
        };
        session.save_to_xml(&path).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(session, loaded);
        assert!(loaded.view.indent_guide);
    }

    /// `ViewSettings::default()` (indent guide off) must NOT emit a
    /// `<view/>` element or an `indent_guide` attribute — same
    /// discipline the other `Option`-typed and `bool`-default fields
    /// on `Session` observe. Reloading with no `<view>` element
    /// yields the default `ViewSettings` back.
    #[test]
    fn empty_view_not_serialized() {
        let (_dir, path) = temp_session_path();
        let session = Session::default();
        session.save_to_xml(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("<view"),
            "<view> element must be elided when default: {text}"
        );
        assert!(
            !text.contains("indent_guide"),
            "indent_guide attribute must be elided when default: {text}"
        );
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(loaded.view, ViewSettings::default());
    }

    /// A session.xml written before the `<view>` feature shipped
    /// must still parse. The loaded `view` field falls back to
    /// `ViewSettings::default()` — no runtime error, no behavioural
    /// change relative to pre-feature.
    #[test]
    fn pre_view_session_xml_loads_with_default_view() {
        let (_dir, path) = temp_session_path();
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<session active="0"><tab path="hello.txt" cursor="0" encoding="UTF-8" eol="LF"/></session>"#;
        std::fs::write(&path, xml).unwrap();
        let loaded = Session::load_from_xml(&path).unwrap();
        assert_eq!(loaded.view, ViewSettings::default());
        assert_eq!(loaded.tabs.len(), 1);
    }
}
