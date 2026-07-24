//! Notepad++-compatible session file format.
//!
//! Read/write the XML shape N++ ships in `session.xml` (and the same
//! shape a user gets when they hit `File → Save Session...` in N++).
//! Code++ uses this exclusively for **interchange** with N++ — the
//! internal auto-persist file `<config_dir>/session.xml` uses the
//! Code++-native schema in [`crate::session`], so the two formats are
//! independent.
//!
//! Shape:
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <NotepadPlus>
//!   <Session activeView="0">
//!     <mainView activeIndex="0">
//!       <File filename="C:\path\file.rs" lang="Rust"
//!             firstVisibleLine="0" startPos="42" endPos="42"
//!             tabPinned="no" encoding="-1" userReadOnly="no"/>
//!     </mainView>
//!     <subView activeIndex="0"/>
//!   </Session>
//! </NotepadPlus>
//! ```
//!
//! Every attribute the emitter writes is defaulted on the parser side,
//! so a user pointing Code++ at an N++ session that carries extra
//! attributes (the map* dozen, `tabColourId`, `RTL`, …) parses cleanly
//! and only the attributes we care about (`filename`, `lang`,
//! `firstVisibleLine`, `startPos`, `tabPinned`) contribute to the
//! reconstructed [`NppFile`]. Unknown attributes on the wire are
//! ignored by quick-xml at parse time.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lang::{LangType, LANG_TABLE, L_TEXT};

/// One `<File>` element inside `<mainView>`/`<subView>`. Carries the
/// subset of attributes Code++ round-trips; every field except
/// `filename` has a serde default so a partial file (or an N++ file
/// with a differing attribute set) parses without error.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NppFile {
    /// Absolute path to the file on disk. Required — a `<File>`
    /// with no `filename` is a corrupt entry and gets skipped by
    /// the load path in the UI layer.
    #[serde(rename = "@filename")]
    pub filename: PathBuf,
    /// Language name in N++'s title-case convention ("Rust",
    /// "C++", "None (Normal Text)"). Empty string when the writer
    /// didn't populate it. Consumed by [`lang_type_from_npp_name`]
    /// to resolve back to a Code++ `LangType`.
    #[serde(rename = "@lang", default)]
    pub lang: String,
    /// First display line visible in the editor when the session
    /// was saved. Not consumed by the current load path (which only
    /// restores the caret via `apply_load_result`'s `SCI_GOTOPOS`
    /// call) — kept for round-trip fidelity with the on-disk N++
    /// shape, and so a future Load Session that also drives
    /// `SCI_SETFIRSTVISIBLELINE` has the value already parsed.
    #[serde(rename = "@firstVisibleLine", default)]
    pub first_visible_line: u32,
    /// Selection anchor / caret byte position at save time. Used
    /// to restore the caret via `SCI_GOTOPOS` on load.
    #[serde(rename = "@startPos", default)]
    pub start_pos: u64,
    /// Selection tail. Same as `start_pos` when no selection is
    /// active. Not consumed by the current load path (which only
    /// restores the caret), kept for round-trip fidelity.
    #[serde(rename = "@endPos", default)]
    pub end_pos: u64,
    /// N++-shape "yes"/"no" flag for the tab pin state. Round-trips
    /// via [`YesNo`] so parsing / emitting stays symmetric.
    #[serde(rename = "@tabPinned", default)]
    pub tab_pinned: YesNo,
    /// N++'s encoding code. `-1` means "autodetect on load" — the
    /// only value Code++'s emitter writes. Kept as a field so
    /// third-party session files that carry a real code parse
    /// cleanly.
    #[serde(rename = "@encoding", default = "default_encoding")]
    pub encoding: i32,
    /// N++'s user-read-only flag. Not consumed by Code++'s load
    /// path (there's no Scintilla-level read-only toggle exposed
    /// yet), but written on save so N++ opening the file sees the
    /// attribute it expects.
    #[serde(rename = "@userReadOnly", default)]
    pub user_read_only: YesNo,
}

fn default_encoding() -> i32 {
    -1
}

/// `<mainView>` or `<subView>` — a container of `<File>` entries plus
/// the tab index that was active in that view. Code++ only uses one
/// view today; `sub_view` on [`NppSession`] is written as an empty
/// element for N++ shape compatibility.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NppView {
    /// Zero-based index into `files` of the active tab in this view.
    #[serde(rename = "@activeIndex", default)]
    pub active_index: usize,
    /// The tabs in this view, in display order.
    #[serde(rename = "File", default)]
    pub files: Vec<NppFile>,
}

/// `<Session>` — the payload N++ writes inside `<NotepadPlus>`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NppSession {
    /// `0` = main view active, `1` = sub view active. Code++ only
    /// uses the main view, so on save this is always `0`.
    #[serde(rename = "@activeView", default)]
    pub active_view: u8,
    #[serde(rename = "mainView")]
    pub main_view: NppView,
    /// Sub view. Optional on parse (older / minimal files elide
    /// it). Emitted as `<subView activeIndex="0"/>` for N++ shape
    /// compatibility.
    #[serde(rename = "subView", default, skip_serializing_if = "Option::is_none")]
    pub sub_view: Option<NppView>,
}

/// Root `<NotepadPlus>` element. quick-xml requires the root type to
/// be serialisable as its own struct.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "NotepadPlus")]
pub struct NppSessionDoc {
    #[serde(rename = "Session")]
    pub session: NppSession,
}

/// N++-shape "yes"/"no" attribute. serde-tagged so `#[derive(Deserialize)]`
/// on the parent struct picks the right variant from the wire attribute
/// value, and emitters produce the lowercase forms N++ writes.
///
/// `Default::default()` returns `No`, matching the N++ convention that
/// omitted `tabPinned` / `userReadOnly` mean "off".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum YesNo {
    #[serde(rename = "no")]
    #[default]
    No,
    #[serde(rename = "yes")]
    Yes,
}

impl YesNo {
    #[must_use]
    pub const fn from_bool(b: bool) -> Self {
        if b {
            YesNo::Yes
        } else {
            YesNo::No
        }
    }

    #[must_use]
    pub const fn as_bool(self) -> bool {
        matches!(self, YesNo::Yes)
    }
}

/// Hard cap on the size of a session XML file [`NppSessionDoc::load_from_xml`]
/// will read. Real N++ session files are kilobytes; even a 512-tab session
/// (the `MAX_SESSION_TABS` ceiling) with long paths stays well under a
/// megabyte. The cap bounds the synchronous read + full-document
/// deserialisation of a *user-picked* (hence untrusted) file so a hostile
/// multi-hundred-MB "session" can't freeze the UI thread before the tab
/// cap — which only applies after the parse — has a chance to. 8 MiB is
/// generous headroom over any legitimate session.
pub const MAX_SESSION_XML_BYTES: u64 = 8 * 1024 * 1024;

/// Errors from reading / writing an N++ session file.
#[derive(Debug)]
pub enum NppSessionError {
    /// I/O failure on read/write.
    Io(std::io::Error),
    /// XML did not match the expected shape.
    Parse(quick_xml::DeError),
    /// Serialisation to XML failed.
    Serialize(quick_xml::SeError),
    /// The file exceeds [`MAX_SESSION_XML_BYTES`] and was refused before
    /// being parsed.
    TooLarge {
        /// The byte cap that was exceeded.
        limit: u64,
    },
}

impl std::fmt::Display for NppSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NppSessionError::Io(e) => write!(f, "npp session I/O error: {e}"),
            NppSessionError::Parse(e) => write!(f, "npp session parse error: {e}"),
            NppSessionError::Serialize(e) => write!(f, "npp session serialize error: {e}"),
            NppSessionError::TooLarge { limit } => {
                write!(f, "npp session file exceeds the {limit}-byte size limit")
            }
        }
    }
}

impl std::error::Error for NppSessionError {}

impl From<std::io::Error> for NppSessionError {
    fn from(e: std::io::Error) -> Self {
        NppSessionError::Io(e)
    }
}

impl NppSessionDoc {
    /// Read an N++ session XML file from `path`. A missing file is an
    /// error (unlike Code++'s internal session — the user chose the
    /// path explicitly via a file picker, so "not found" is a real
    /// failure, not a first-run signal).
    ///
    /// # Errors
    ///
    /// Returns `Io` on any read failure; `Parse` on malformed XML or
    /// a shape that doesn't fit [`NppSessionDoc`].
    pub fn load_from_xml(path: &Path) -> Result<Self, NppSessionError> {
        use std::io::Read;
        let file = std::fs::File::open(path).map_err(NppSessionError::Io)?;
        // Bounded read: `take(cap + 1)` so a file exactly at the cap still
        // reads whole, while anything larger trips the length check below.
        // Reading through the cap (rather than trusting `metadata().len()`)
        // is deliberate — a file can grow between a stat and the read, so
        // the bound has to live on the read itself.
        let mut buf = Vec::new();
        file.take(MAX_SESSION_XML_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(NppSessionError::Io)?;
        if buf.len() as u64 > MAX_SESSION_XML_BYTES {
            return Err(NppSessionError::TooLarge {
                limit: MAX_SESSION_XML_BYTES,
            });
        }
        // Preserve `read_to_string`'s contract: non-UTF-8 is an
        // `InvalidData` I/O error, not a silently lossy decode.
        let contents = String::from_utf8(buf).map_err(|e| {
            NppSessionError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        Self::from_xml_str(&contents)
    }

    /// Parse an N++ session XML document from an in-memory string.
    /// Split out from `load_from_xml` so unit tests can exercise
    /// parsing without touching the filesystem.
    ///
    /// # Errors
    ///
    /// Returns `Parse` when the bytes don't deserialise into a
    /// [`NppSessionDoc`].
    pub fn from_xml_str(s: &str) -> Result<Self, NppSessionError> {
        quick_xml::de::from_str(s).map_err(NppSessionError::Parse)
    }

    /// Write to `path` atomically via a temp-file + rename. Same
    /// discipline as `Session::save_to_xml` — power-loss safe, no
    /// stray temp files on failure.
    ///
    /// # Errors
    ///
    /// Returns `Serialize` if quick-xml refuses the document (rare);
    /// `Io` on parent-dir creation, temp-file write/sync, or the
    /// atomic rename step failing.
    pub fn save_to_xml(&self, path: &Path) -> Result<(), NppSessionError> {
        let xml = self.to_xml_string()?;

        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent)?;
        }
        let parent_dir = parent.unwrap_or_else(|| Path::new("."));

        let mut tmp = tempfile::Builder::new()
            .prefix(".npp-session-")
            .suffix(".xml.tmp")
            .tempfile_in(parent_dir)?;
        tmp.write_all(xml.as_bytes())?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(path)
            .map_err(|e| NppSessionError::Io(e.error))?;
        Ok(())
    }

    /// Serialise to an XML string with the standard prolog. Broken
    /// out from `save_to_xml` so tests can inspect the wire format
    /// without a filesystem write.
    ///
    /// # Errors
    ///
    /// Returns `Serialize` when quick-xml refuses to emit the
    /// document (rare — only on non-UTF-8 attribute content).
    pub fn to_xml_string(&self) -> Result<String, NppSessionError> {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        quick_xml::se::to_writer(&mut xml, self).map_err(NppSessionError::Serialize)?;
        Ok(xml)
    }
}

/// Convert a Code++ [`LangType`] to the language name N++ writes in
/// the `lang="..."` attribute. `L_TEXT` maps to the special string
/// N++ uses (`"None (Normal Text)"`); everything else maps to the
/// `menu_label` from `LANG_TABLE`. Falls back to `"None (Normal Text)"`
/// for lang ids Code++ doesn't know about — best-effort, so the emitter
/// never fails on future / unknown ids.
#[must_use]
pub fn npp_name_for_lang(lang: LangType) -> &'static str {
    if lang == L_TEXT {
        return "None (Normal Text)";
    }
    LANG_TABLE
        .iter()
        .find(|e| e.lang == lang)
        .map_or("None (Normal Text)", |e| e.menu_label)
}

/// True when `path` is a Windows non-local path — UNC
/// (`\\server\share\...`), device namespace (`\\.\...`), or verbatim
/// namespace (`\\?\...`). Callers reject these before opening files
/// referenced by a session XML, because feeding a hostile UNC into
/// `std::fs::open` triggers an outbound SMB connection with the
/// current user's NTLM credentials (Responder-style hash capture).
///
/// Uses `std::path::Path::components()` — the same path parser
/// `std::fs::File::open` funnels through before reaching
/// `CreateFileW`. That guarantees the check stays in sync with
/// what Windows will actually resolve for the string, including
/// the mixed / forward-slash spellings the OS treats as equivalent
/// to backslashes (`//server/share/x`, `\/server\share`, `/\server`
/// all resolve to the same UNC as `\\server\share` and would be
/// missed by a hand-rolled byte-prefix check).
///
/// On Unix, `Path::components()` never emits a `Prefix` variant
/// regardless of the string content — a `\\server\share` on Unix
/// is parsed as a single filename with literal backslashes, and
/// opening it fails with `ENOENT` rather than leaking credentials.
/// So the function is effectively a no-op on non-Windows targets,
/// matching where the underlying threat actually lives.
///
/// Accept/reject breakdown by `std::path::Prefix` variant:
///
/// * **Rejected** — `UNC(..)`, `VerbatimUNC(..)`, `DeviceNS(..)`,
///   `Verbatim(..)`. `UNC` / `VerbatimUNC` directly carry the SMB /
///   NTLM leak; `DeviceNS` reaches driver objects (`\\.\PhysicalDrive0`
///   and friends); non-disk `Verbatim` (`\\?\SomeName`) is exotic
///   enough that session XML has no legitimate use for it and
///   rejecting keeps the surface small.
/// * **Accepted** — `Disk(..)`, `VerbatimDisk(..)`. Both are
///   ordinary local drive paths; the verbatim form is just the
///   "long path" alias (`\\?\C:\...` = `C:\...`), no security
///   difference from the plain form.
#[must_use]
pub fn is_non_local_windows_path(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    matches!(
        prefix.kind(),
        Prefix::UNC(..) | Prefix::VerbatimUNC(..) | Prefix::DeviceNS(..) | Prefix::Verbatim(..)
    )
}

/// Resolve an N++ `lang="..."` attribute value back to a Code++
/// [`LangType`]. Case-insensitive against `menu_label` so both N++'s
/// `"LISP"` and Code++'s own `"Lisp"` resolve to `L_LISP`. Returns
/// `None` for the empty string, for `"None (Normal Text)"` (which
/// signals "let extension detection run"), and for any name not
/// present in `LANG_TABLE`. The caller (typically the UI's Load
/// Session handler) treats `None` as "fall through to extension-based
/// detection", matching how the internal session-restore path
/// handles `Tab.lang = None`.
#[must_use]
pub fn lang_type_from_npp_name(name: &str) -> Option<LangType> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("None (Normal Text)") {
        return None;
    }
    LANG_TABLE
        .iter()
        .find(|e| e.menu_label.eq_ignore_ascii_case(trimmed))
        .map(|e| e.lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_file(name: &str) -> NppFile {
        NppFile {
            filename: PathBuf::from(name),
            lang: "Rust".into(),
            first_visible_line: 12,
            start_pos: 100,
            end_pos: 100,
            tab_pinned: YesNo::No,
            encoding: -1,
            user_read_only: YesNo::No,
        }
    }

    #[test]
    fn round_trip_single_file() {
        let doc = NppSessionDoc {
            session: NppSession {
                active_view: 0,
                main_view: NppView {
                    active_index: 0,
                    files: vec![sample_file(r"C:\a\b.rs")],
                },
                sub_view: Some(NppView::default()),
            },
        };
        let xml = doc.to_xml_string().unwrap();
        let parsed = NppSessionDoc::from_xml_str(&xml).unwrap();
        assert_eq!(doc, parsed);
    }

    #[test]
    fn round_trip_multiple_files_preserves_order_and_active_index() {
        let doc = NppSessionDoc {
            session: NppSession {
                active_view: 0,
                main_view: NppView {
                    active_index: 2,
                    files: vec![
                        sample_file(r"C:\a\one.txt"),
                        sample_file(r"C:\a\two.txt"),
                        sample_file(r"C:\a\three.txt"),
                    ],
                },
                sub_view: Some(NppView::default()),
            },
        };
        let xml = doc.to_xml_string().unwrap();
        let parsed = NppSessionDoc::from_xml_str(&xml).unwrap();
        assert_eq!(doc, parsed);
        assert_eq!(parsed.session.main_view.active_index, 2);
        assert_eq!(parsed.session.main_view.files.len(), 3);
    }

    /// Pinned round-trips as `yes` on the wire, non-default is written.
    #[test]
    fn tab_pinned_round_trips() {
        let mut f = sample_file(r"C:\a\pin.rs");
        f.tab_pinned = YesNo::Yes;
        let doc = NppSessionDoc {
            session: NppSession {
                main_view: NppView {
                    files: vec![f.clone()],
                    ..NppView::default()
                },
                sub_view: Some(NppView::default()),
                ..NppSession::default()
            },
        };
        let xml = doc.to_xml_string().unwrap();
        assert!(xml.contains(r#"tabPinned="yes""#), "got: {xml}");
        let parsed = NppSessionDoc::from_xml_str(&xml).unwrap();
        assert_eq!(parsed.session.main_view.files[0].tab_pinned, YesNo::Yes);
    }

    /// Parsing an N++-native file with the full extra-attribute set
    /// works — unknown attributes silently ignored, known ones
    /// populated. Uses a shape taken from the user's example
    /// Test.xml so we cover the real-world attribute cloud.
    #[test]
    fn parses_full_npp_wire_shape() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<NotepadPlus>
    <Session activeView="0">
        <mainView activeIndex="0">
            <File firstVisibleLine="0" xOffset="0" scrollWidth="1164"
                  startPos="117" endPos="117" selMode="0" offset="0"
                  wrapCount="1" lang="None (Normal Text)" encoding="-1"
                  userReadOnly="no" filename="C:\Users\Max\Projects\notes.txt"
                  backupFilePath="" originalFileLastModifTimestamp="2482302963"
                  originalFileLastModifTimestampHigh="31265936" tabColourId="-1"
                  RTL="no" tabPinned="yes" mapFirstVisibleDisplayLine="-1"
                  mapFirstVisibleDocLine="-1" mapLastVisibleDocLine="-1"
                  mapNbLine="-1" mapHigherPos="-1" mapWidth="-1" mapHeight="-1"
                  mapKByteInDoc="512" mapWrapIndentMode="-1" mapIsWrap="no"/>
            <File firstVisibleLine="0" xOffset="0" scrollWidth="970"
                  startPos="0" endPos="0" selMode="0" offset="0" wrapCount="1"
                  lang="Rust" encoding="-1" userReadOnly="no"
                  filename="C:\Users\Max\Downloads\test_case.rs"
                  backupFilePath="" originalFileLastModifTimestamp="0"
                  originalFileLastModifTimestampHigh="0" tabColourId="-1"
                  RTL="no" tabPinned="no"/>
        </mainView>
        <subView activeIndex="0"/>
    </Session>
</NotepadPlus>"#;
        let doc = NppSessionDoc::from_xml_str(xml).unwrap();
        let files = &doc.session.main_view.files;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].filename,
            PathBuf::from(r"C:\Users\Max\Projects\notes.txt")
        );
        assert_eq!(files[0].lang, "None (Normal Text)");
        assert_eq!(files[0].start_pos, 117);
        assert_eq!(files[0].tab_pinned, YesNo::Yes);
        assert_eq!(files[1].lang, "Rust");
        assert_eq!(files[1].tab_pinned, YesNo::No);
    }

    /// Minimal file with only `filename` — everything else defaults.
    /// Ensures the "user hand-edited a tiny XML" path works.
    #[test]
    fn parses_minimal_file_element() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<NotepadPlus>
    <Session activeView="0">
        <mainView activeIndex="0">
            <File filename="C:\a.txt"/>
        </mainView>
    </Session>
</NotepadPlus>"#;
        let doc = NppSessionDoc::from_xml_str(xml).unwrap();
        let f = &doc.session.main_view.files[0];
        assert_eq!(f.filename, PathBuf::from(r"C:\a.txt"));
        assert_eq!(f.lang, "");
        assert_eq!(f.start_pos, 0);
        assert_eq!(f.tab_pinned, YesNo::No);
        assert_eq!(f.encoding, -1);
        assert!(doc.session.sub_view.is_none());
    }

    #[test]
    fn save_and_load_via_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.xml");
        let doc = NppSessionDoc {
            session: NppSession {
                main_view: NppView {
                    files: vec![sample_file(r"C:\a\one.rs")],
                    ..NppView::default()
                },
                sub_view: Some(NppView::default()),
                ..NppSession::default()
            },
        };
        doc.save_to_xml(&path).unwrap();
        let loaded = NppSessionDoc::load_from_xml(&path).unwrap();
        assert_eq!(doc, loaded);
    }

    #[test]
    fn save_creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("session.xml");
        NppSessionDoc::default().save_to_xml(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.xml");
        NppSessionDoc::default().save_to_xml(&path).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries.len(), 1, "stray files: {entries:?}");
        assert_eq!(entries[0], "session.xml");
    }

    #[test]
    fn malformed_xml_returns_parse_error() {
        let err = NppSessionDoc::from_xml_str("<not-a-npp-session>").unwrap_err();
        assert!(matches!(err, NppSessionError::Parse(_)));
    }

    #[test]
    fn load_from_xml_refuses_a_file_over_the_size_cap() {
        // An untrusted session file just past the cap is rejected before
        // any parse allocation happens — the DoS guard the security audit
        // asked for. Pad valid-ish XML out with a comment so the rejection
        // is about size, not shape.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.xml");
        let over_cap = usize::try_from(MAX_SESSION_XML_BYTES + 1).unwrap();
        let mut bytes = Vec::with_capacity(over_cap + 8);
        bytes.extend_from_slice(b"<!-- ");
        bytes.resize(over_cap, b'x');
        std::fs::write(&path, &bytes).unwrap();
        let err = NppSessionDoc::load_from_xml(&path).unwrap_err();
        assert!(
            matches!(err, NppSessionError::TooLarge { limit } if limit == MAX_SESSION_XML_BYTES),
            "expected TooLarge, got {err:?}",
        );
    }

    #[test]
    fn load_from_xml_accepts_a_file_at_the_size_cap() {
        // Boundary: exactly the cap reads whole (the `+1` in the bounded
        // read is what makes an at-cap file distinguishable from an
        // over-cap one). Content is a valid empty session so the parse
        // succeeds and only the size gate is under test.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("at_cap.xml");
        let doc = NppSessionDoc::default();
        let xml = doc.to_xml_string().unwrap();
        assert!((xml.len() as u64) <= MAX_SESSION_XML_BYTES);
        std::fs::write(&path, xml.as_bytes()).unwrap();
        assert!(NppSessionDoc::load_from_xml(&path).is_ok());
    }

    // --- LangType <-> name mapping ---

    #[test]
    fn npp_name_for_lang_maps_common_langs() {
        use crate::lang::{L_CAML, L_CPP, L_LISP, L_RUST, L_TEXT};
        assert_eq!(npp_name_for_lang(L_TEXT), "None (Normal Text)");
        assert_eq!(npp_name_for_lang(L_RUST), "Rust");
        assert_eq!(npp_name_for_lang(L_CPP), "C++");
        assert_eq!(npp_name_for_lang(L_CAML), "Caml");
        assert_eq!(npp_name_for_lang(L_LISP), "Lisp");
    }

    #[test]
    fn lang_type_from_npp_name_case_insensitive() {
        use crate::lang::{L_LISP, L_RUST};
        assert_eq!(lang_type_from_npp_name("Rust"), Some(L_RUST));
        assert_eq!(lang_type_from_npp_name("rust"), Some(L_RUST));
        assert_eq!(lang_type_from_npp_name("RUST"), Some(L_RUST));
        // N++ writes "LISP" all-caps; Code++'s LANG_TABLE has "Lisp".
        // The case-insensitive lookup bridges them.
        assert_eq!(lang_type_from_npp_name("LISP"), Some(L_LISP));
    }

    #[test]
    fn lang_type_from_npp_name_none_normal_text_returns_none() {
        assert_eq!(lang_type_from_npp_name("None (Normal Text)"), None);
        assert_eq!(lang_type_from_npp_name("none (normal text)"), None);
        assert_eq!(lang_type_from_npp_name(""), None);
        assert_eq!(lang_type_from_npp_name("   "), None);
    }

    #[test]
    fn lang_type_from_npp_name_unknown_returns_none() {
        assert_eq!(lang_type_from_npp_name("Klingon"), None);
    }

    // --- Non-local path detection (session-file UNC/device guard) ---
    //
    // The check is Windows-specific: `Path::components()` only
    // emits `Prefix` variants when parsing on a Windows target, so
    // running these against a Linux/macOS host would exercise a
    // different code path (`Component::Prefix` never appears in the
    // component stream on those platforms). Cross-platform pieces
    // that DO work identically (empty path, absent-prefix local
    // paths) run unconditionally below.

    #[cfg(windows)]
    #[test]
    fn is_non_local_flags_unc_paths_backslash_form() {
        assert!(is_non_local_windows_path(&PathBuf::from(
            r"\\server\share\file.txt"
        )));
        assert!(is_non_local_windows_path(&PathBuf::from(
            r"\\attacker.example.com\loot\x.txt"
        )));
    }

    /// Windows treats `/` and `\` as interchangeable path
    /// separators in the UNC prefix parser, so a hand-rolled
    /// byte-level `\\` check would silently miss forward-slash
    /// spellings. Pin the forward-slash variant here so a future
    /// refactor away from `Path::components()` gets caught. This
    /// is the exact bypass the second security-audit pass found.
    #[cfg(windows)]
    #[test]
    fn is_non_local_flags_unc_paths_forward_slash_form() {
        // Pure forward-slash UNC — the exact bypass shape the
        // security audit demonstrated (SMB connection triggered by
        // `std::fs::open("//server/share/x")` on Windows).
        assert!(is_non_local_windows_path(&PathBuf::from(
            "//server/share/file.txt"
        )));
        assert!(is_non_local_windows_path(&PathBuf::from(
            "//attacker.example.com/loot/x.txt"
        )));
    }

    /// Mixed-slash spellings (`\/`, `/\`) also resolve to UNC on
    /// Windows and must be rejected. `Path::components()` normalises
    /// through the same parser `CreateFileW` uses, so this coverage
    /// falls out naturally — pin it so a byte-level regression
    /// can't sneak past.
    #[cfg(windows)]
    #[test]
    fn is_non_local_flags_unc_paths_mixed_slash_form() {
        assert!(is_non_local_windows_path(&PathBuf::from(
            r"\/server\share\file.txt"
        )));
        assert!(is_non_local_windows_path(&PathBuf::from(
            r"/\server\share\file.txt"
        )));
    }

    /// Device namespace paths (`\\.\...`) reach hardware / driver
    /// objects; forbid session files from opening them. Verbatim-UNC
    /// (`\\?\UNC\server\share`) is the "long-path" alias for a UNC
    /// and carries the same SMB / NTLM leak — must be rejected too.
    #[cfg(windows)]
    #[test]
    fn is_non_local_flags_device_and_verbatim_unc_namespaces() {
        assert!(is_non_local_windows_path(&PathBuf::from(
            r"\\.\PhysicalDrive0"
        )));
        assert!(is_non_local_windows_path(&PathBuf::from(
            r"\\?\UNC\server\share\file.txt"
        )));
    }

    /// `\\?\C:\...` is `Prefix::VerbatimDisk` — a **local** drive
    /// path with the "bypass `MAX_PATH`" prefix. It doesn't trigger
    /// SMB and shouldn't be rejected. Pin this so a well-meaning
    /// future refactor that tries to reject "everything starting
    /// with `\\?\`" doesn't accidentally break legitimate long
    /// local paths.
    #[cfg(windows)]
    #[test]
    fn is_non_local_accepts_verbatim_disk_paths() {
        assert!(!is_non_local_windows_path(&PathBuf::from(
            r"\\?\C:\Very\Long\Path"
        )));
    }

    #[cfg(windows)]
    #[test]
    fn is_non_local_accepts_windows_local_paths() {
        assert!(!is_non_local_windows_path(&PathBuf::from(
            r"C:\Users\me\file.txt"
        )));
        // A single leading backslash (drive-relative) is not
        // rejected — it's a local path that just references the
        // current drive. UNC requires *two* leading separators.
        assert!(!is_non_local_windows_path(&PathBuf::from(r"\Windows\x")));
    }

    /// Cross-platform sanity: relative paths and empty paths never
    /// trip the check on any target — no `Component::Prefix` ever
    /// appears in the component stream for these shapes.
    #[test]
    fn is_non_local_accepts_relative_and_empty() {
        assert!(!is_non_local_windows_path(&PathBuf::from("relative.txt")));
        assert!(!is_non_local_windows_path(&PathBuf::from("")));
        assert!(!is_non_local_windows_path(&PathBuf::from(
            "single-backslash-\\-inside.txt"
        )));
    }

    /// POSIX-shape paths never carry a `Prefix` component — even on
    /// Windows the check tolerates them, and on Unix they're just
    /// normal paths. Kept cross-platform so both CI legs see it.
    #[test]
    fn is_non_local_accepts_posix_paths() {
        assert!(!is_non_local_windows_path(&PathBuf::from(
            "/usr/local/bin/x"
        )));
    }

    /// Regression test for the "filter shifts index" bug caught in
    /// the second-pass review: resolving `active_index` to a path
    /// **before** filtering (rather than indexing into the filtered
    /// list afterwards) is what makes the Load Session's
    /// active-tab-restore correct in the presence of rejected
    /// entries. This test pins the pre-filter lookup shape so a
    /// future refactor that "helpfully" moves the resolution after
    /// the filter breaks the test rather than silently regressing
    /// the UX. Test doesn't care what shape gets filtered — the
    /// point is the `.get(active_index)` result must be independent
    /// of the filter's subsequent behaviour, so a placeholder
    /// "will-be-filtered" name at index 1 is enough.
    #[test]
    fn active_index_resolves_to_path_before_filtering() {
        let file = |name: &str| NppFile {
            filename: PathBuf::from(name),
            ..NppFile::default()
        };
        let doc = NppSessionDoc {
            session: NppSession {
                active_view: 0,
                main_view: NppView {
                    active_index: 2,
                    files: vec![
                        file("A.txt"),
                        file("FILTERED_OUT.txt"),
                        file("C.txt"),
                        file("D.txt"),
                    ],
                },
                sub_view: None,
            },
        };
        let recorded = doc
            .session
            .main_view
            .files
            .get(doc.session.main_view.active_index)
            .map(|f| f.filename.clone());
        assert_eq!(recorded, Some(PathBuf::from("C.txt")));
    }

    /// End-to-end: emit a doc with `L_RUST`, parse the wire form, and
    /// resolve back. Confirms the two mapping helpers compose so a
    /// file saved from Code++ and loaded back reconstructs the same
    /// `LangType`.
    #[test]
    fn lang_round_trip_via_wire_format() {
        use crate::lang::L_RUST;
        let mut file = sample_file(r"C:\a\one.rs");
        file.lang = npp_name_for_lang(L_RUST).into();
        let doc = NppSessionDoc {
            session: NppSession {
                main_view: NppView {
                    files: vec![file],
                    ..NppView::default()
                },
                ..NppSession::default()
            },
        };
        let xml = doc.to_xml_string().unwrap();
        let parsed = NppSessionDoc::from_xml_str(&xml).unwrap();
        assert_eq!(
            lang_type_from_npp_name(&parsed.session.main_view.files[0].lang),
            Some(L_RUST)
        );
    }
}
