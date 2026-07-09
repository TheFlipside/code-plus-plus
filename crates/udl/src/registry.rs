//! Runtime UDL registry — the in-memory collection of every
//! `UdlDefinition` loaded at startup from
//! `<config_dir>/userDefineLangs/*.udl.xml`, each carrying its own
//! dynamically-assigned `LangType` id.
//!
//! This module is Phase 4.6 m1b: the scanner + registry
//! infrastructure. The Shell layer instantiates one at startup;
//! the UI (Phase 4.6 m1d) reads the entries to append them to the
//! Language menu; the container-lexer runtime (Phase 4.6 m1c)
//! looks up a UDL by its assigned id when the user activates a
//! UDL-language buffer.

use std::path::{Path, PathBuf};

use crate::UdlDefinition;

/// Base `LangType` numeric id for dynamically-registered UDLs.
///
/// The built-in language constant range tops out at
/// `L_EXTERNAL = 93` per the N++ public plugin ABI
/// (`plugins/nppcompat-headers/Notepad_plus_msgs.h:846`). Starting
/// UDL ids at `1024` leaves ~930 slots for future N++ additions
/// (well past N++'s ~1-per-year growth rate) before any
/// collision.
///
/// **These ids never leave Code++'s process boundary.** When a
/// plugin queries a UDL buffer via `NPPM_GETCURRENTLANGTYPE`, the
/// plugin-host dispatcher returns `L_USER = 15` — matching
/// Notepad++'s public API contract for "buffer uses a UDL." The
/// UDL-specific id is a Code++-internal detail for distinguishing
/// which UDL applies to which buffer, not something plugins see.
pub const UDL_LANG_TYPE_BASE: i32 = 1024;

/// End of the UDL dynamic-id space (inclusive). 1024 slots is
/// comfortably past any realistic user's collection size — N++'s
/// own community-curated UDL list carries ~50 entries and
/// Edditoria's `markdown-plus-plus` adds a further dozen; 1024 is
/// 15-20× over.
pub const UDL_LANG_TYPE_END: i32 = 2047;

/// Enumeration cap on the number of directory entries the
/// scanner will inspect. Distinct from the id-space size —
/// enumeration + canonicalization happens BEFORE id assignment,
/// so a directory containing millions of `.udl.xml`-named files
/// (planted by a hostile install script or by accident) would
/// otherwise force millions of synchronous `canonicalize`
/// syscalls on the `Shell::new` startup path, blowing past the
/// DESIGN.md §8 cold-start budget (<80 ms). Set to the id-space
/// size — the scanner can't load more UDLs than there are ids
/// for anyway, so bounding enumeration to the same limit doesn't
/// discard any file that could have loaded.
const MAX_UDL_SCAN_ENTRIES: usize = (UDL_LANG_TYPE_END - UDL_LANG_TYPE_BASE + 1) as usize;

/// One registered UDL — the parsed definition plus the dynamic
/// `LangType` id it was assigned at scan time.
///
/// **ID stability caveat.** Current assignment is sequential in
/// alphabetically-sorted scan order (see
/// [`UdlRegistry::scan_dir`]). A user adding, removing, or
/// renaming a UDL file mid-alphabet renumbers every entry that
/// sorts after the change. Session-restore across a renumbering
/// event will resolve a stored `LangType(id)` to a **different**
/// UDL than the one active when the session was saved. Fix
/// (deferred to Phase 4.6 m1d): store UDL identity by
/// `definition.name` in `session.xml` and look up by name on
/// restore, treating the numeric id as ephemeral.
///
/// `#[non_exhaustive]` so m1d / m3 can add fields (e.g. a menu-
/// item command id derived from `lang_type_id`) without a
/// match-exhaustiveness churn commit at every call site.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct UdlEntry {
    /// Dynamic `LangType` id assigned by
    /// [`UdlRegistry::scan_dir`]. Guaranteed to be in
    /// [`UDL_LANG_TYPE_BASE`]`..=`[`UDL_LANG_TYPE_END`].
    pub lang_type_id: i32,
    /// The parsed UDL. `definition.source_path` carries the file
    /// path the scanner loaded from.
    pub definition: UdlDefinition,
}

/// Filter one directory entry to a canonicalised, contained
/// UDL path, appending to `paths` on success or logging on
/// skip.
///
/// Extracted from [`UdlRegistry::scan_dir`]'s loop body so the
/// scanner's per-iteration work stays short and so a future
/// unit test can drive the classifier in isolation. Preserves
/// the security discipline:
/// 1. Only `.udl.xml`-suffixed filenames enter canonicalisation.
/// 2. Both `canonicalize(entry)` failure and out-of-directory
///    resolution log-and-skip (fail closed).
/// 3. The canonical path — not the raw entry path — is pushed,
///    which is what closes the TOCTOU window between the
///    containment check and the eventual `File::open` inside
///    `UdlDefinition::from_file`.
fn classify_entry(
    entry_res: std::io::Result<std::fs::DirEntry>,
    canonical_dir: &Path,
    paths: &mut Vec<PathBuf>,
) {
    let entry = match entry_res {
        Ok(e) => e,
        Err(err) => {
            tracing::debug!(
                error = %err,
                "userDefineLangs directory entry read error; skipped"
            );
            return;
        }
    };
    let raw_path = entry.path();
    let is_udl = raw_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".udl.xml"));
    if !is_udl {
        return;
    }
    // TOCTOU-safe path resolution — see the caller's inline
    // comment for the rationale. `canonicalize` also resolves
    // NTFS mount points / reparse points, not just symlinks.
    match std::fs::canonicalize(&raw_path) {
        Ok(cpath) if cpath.starts_with(canonical_dir) => {
            paths.push(cpath);
        }
        Ok(cpath) => {
            tracing::warn!(
                path = ?raw_path,
                resolved = ?cpath,
                dir = ?canonical_dir,
                "UDL entry resolves outside userDefineLangs directory; \
                 skipped"
            );
        }
        Err(err) => {
            tracing::warn!(
                path = ?raw_path,
                error = %err,
                "UDL entry failed to canonicalise; skipped"
            );
        }
    }
}

/// Runtime-registered UDLs. Owned by the Shell layer; populated
/// once at startup by [`UdlRegistry::scan_dir`] against
/// `<config_dir>/userDefineLangs/`.
///
/// The registry is intentionally read-only after construction —
/// the UDL editor modal (Phase 4.6 m3) writes new UDL files to
/// the directory but does not mutate the in-memory registry;
/// picking up the new file requires re-scanning (also m3).
#[derive(Debug, Clone, Default)]
pub struct UdlRegistry {
    entries: Vec<UdlEntry>,
}

impl UdlRegistry {
    /// Construct an empty registry. Every `scan_dir` call also
    /// returns via the same `Self { entries: ... }` shape, so
    /// callers don't need this in the common path.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `dir` for `*.udl.xml` files and load each one via
    /// [`UdlDefinition::from_file`].
    ///
    /// **Missing / unreadable directory returns an empty
    /// registry.** A fresh install has no `userDefineLangs/`
    /// directory until Phase 4.6 m1d's first-run preinstalled-UDL
    /// copy has landed, and reporting that as an error would
    /// force every startup-path caller to add a `None`-tolerating
    /// branch. The `info`-level log entry is enough for
    /// `--verbose` diagnostics.
    ///
    /// **Per-file failures are logged and skipped.** A single
    /// malformed UDL doesn't prevent the rest of the directory
    /// from loading — same "graceful degradation" discipline as
    /// the plugin loader.
    ///
    /// **Scan order.** Filesystem-order enumeration followed by
    /// an alphabetical sort on the full path before id
    /// assignment. This makes id assignment deterministic across
    /// boots given the same set of files, at the cost of
    /// renumbering when the file set changes (see [`UdlEntry`]'s
    /// stability caveat).
    ///
    /// **`.udl.xml` extension matching.** `Path::extension()`
    /// only returns the last dot-suffix — for `markdown._pre.udl.xml`
    /// it returns `xml`, not `udl.xml`. We test the full filename
    /// against the compound suffix so a user renaming an
    /// unrelated `.xml` file into the directory doesn't get
    /// silently loaded as a UDL.
    ///
    /// **Dynamic-id-space exhaustion.** If more than
    /// [`UDL_LANG_TYPE_END`]` - `[`UDL_LANG_TYPE_BASE`]` + 1`
    /// files load successfully (1024 slots), remaining files are
    /// dropped with a `tracing::warn`. Reaching this limit
    /// requires a truly pathological collection (~20× the largest
    /// realistic bundle); the warning gives future-us a
    /// diagnostic when it happens.
    #[must_use]
    pub fn scan_dir(dir: &Path) -> Self {
        let read_dir = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(err) => {
                tracing::info!(
                    path = ?dir,
                    error = %err,
                    "userDefineLangs directory not readable; \
                     no UDLs loaded"
                );
                return Self::default();
            }
        };
        // Canonicalise `dir` once so we can containment-check
        // each entry against it. If canonicalisation fails
        // (network path, stale junction, drive substitution, etc.)
        // we **fail closed**: skip the whole scan rather than
        // silently accept every entry without a containment check.
        // Falling open here would let an attacker who influences
        // whether `dir` canonicalises (a parent-path symlink, a
        // subst'd drive) turn off symlink defence for every file
        // in the directory.
        let canonical_dir = match std::fs::canonicalize(dir) {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(
                    path = ?dir,
                    error = %err,
                    "userDefineLangs directory failed to canonicalise; \
                     skipping scan to avoid disabling symlink defence"
                );
                return Self::default();
            }
        };

        // Collect + sort paths first so id assignment is
        // deterministic. Bounded by directory listing; each entry
        // is small enough that pre-collecting is fine. Per-entry
        // read errors (permission denied on one file, transient
        // race) are logged at debug — non-actionable but leaves a
        // trace when a user reports "my UDL isn't showing up."
        let mut paths: Vec<PathBuf> = Vec::new();
        // Enumeration is capped so a directory containing
        // hundreds of thousands of `.udl.xml`-named entries
        // (planted by a hostile install script) can't force
        // hundreds of thousands of `canonicalize` syscalls on
        // the startup path — that would blow past DESIGN.md §8's
        // <80 ms cold-start budget.
        //
        // `by_ref().take(...)` limits the sub-iterator but leaves
        // `read_dir` consumable after, so a `.next()` probe below
        // detects whether the cap was actually reached (there was
        // at least one more entry beyond the cap).
        let mut read_dir = read_dir;
        let mut capped_iter = read_dir.by_ref().take(MAX_UDL_SCAN_ENTRIES);
        for entry_res in &mut capped_iter {
            classify_entry(entry_res, &canonical_dir, &mut paths);
        }
        if read_dir.next().is_some() {
            tracing::warn!(
                cap = MAX_UDL_SCAN_ENTRIES,
                "userDefineLangs enumeration cap reached; \
                 stopping scan (remaining entries dropped)"
            );
        }
        // Ordinal path-byte sort (not case-folded human-alphabetical
        // — `"Zebra.udl.xml"` sorts before `"apple.udl.xml"` on
        // ASCII/UTF-16 code points). Adequate for id-assignment
        // determinism, which is the load-bearing property; the UI
        // (m1d) sorts menu entries by display name separately.
        paths.sort();

        let mut entries: Vec<UdlEntry> = Vec::with_capacity(paths.len());
        let mut next_id = UDL_LANG_TYPE_BASE;
        for (i, path) in paths.iter().enumerate() {
            if next_id > UDL_LANG_TYPE_END {
                tracing::warn!(
                    dropped = paths.len() - i,
                    limit = UDL_LANG_TYPE_END - UDL_LANG_TYPE_BASE + 1,
                    "UDL dynamic-id space exhausted; \
                     dropping remaining UDLs from scan"
                );
                break;
            }
            match UdlDefinition::from_file(path) {
                Ok(definition) => {
                    entries.push(UdlEntry {
                        lang_type_id: next_id,
                        definition,
                    });
                    next_id += 1;
                }
                Err(err) => {
                    tracing::warn!(
                        path = ?path,
                        error = %err,
                        "failed to parse UDL; skipped"
                    );
                }
            }
        }
        Self { entries }
    }

    /// Read-only view of the registered UDLs, in dynamic-id
    /// order.
    #[must_use]
    pub fn entries(&self) -> &[UdlEntry] {
        &self.entries
    }

    /// Look up a UDL by its assigned dynamic `LangType` id.
    /// Returns `None` if no entry has that id (either the id is
    /// outside [`UDL_LANG_TYPE_BASE`]`..=`[`UDL_LANG_TYPE_END`]
    /// or the registry never registered a UDL at that slot).
    #[must_use]
    pub fn find_by_lang_type_id(&self, id: i32) -> Option<&UdlEntry> {
        self.entries.iter().find(|e| e.lang_type_id == id)
    }

    /// Case-insensitive extension lookup. Returns the first UDL
    /// that claims `ext` in its `<UserLang ext="...">` list.
    ///
    /// Two UDLs claiming the same extension is a user-config
    /// issue — same behavior as N++ (first match wins). No
    /// runtime error; a user encountering ambiguous highlighting
    /// on a shared extension should rename or delete one of the
    /// conflicting UDL files.
    #[must_use]
    pub fn find_by_extension(&self, ext: &str) -> Option<&UdlEntry> {
        let lower = ext.to_ascii_lowercase();
        self.entries
            .iter()
            .find(|e| e.definition.extensions.iter().any(|x| x == &lower))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to the preinstalled markdown UDL fixture bundled
    /// under `assets/preinstalled-udls/`. Resolved relative to
    /// `CARGO_MANIFEST_DIR` so tests work from any working
    /// directory.
    fn markdown_fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("preinstalled-udls")
            .join("markdown._preinstalled.udl.xml")
    }

    #[test]
    fn empty_dir_yields_empty_registry() {
        let tmp = tempfile::tempdir().expect("tempdir must succeed");
        let reg = UdlRegistry::scan_dir(tmp.path());
        assert!(reg.entries().is_empty());
    }

    #[test]
    fn missing_dir_yields_empty_registry_not_panic() {
        // Fresh-install path: `<config_dir>/userDefineLangs/`
        // doesn't exist yet. Scanner must return an empty
        // registry rather than panic or error, so startup-path
        // callers don't need a None-tolerating branch.
        let missing = Path::new("does/not/exist/anywhere");
        let reg = UdlRegistry::scan_dir(missing);
        assert!(reg.entries().is_empty());
    }

    #[test]
    fn scans_udl_xml_files_only() {
        // Populate a tempdir with one real UDL and a decoy `.xml`
        // file — the scanner must load the UDL and skip the
        // decoy.
        let tmp = tempfile::tempdir().expect("tempdir must succeed");
        let markdown = std::fs::read_to_string(markdown_fixture_path())
            .expect("markdown fixture must be readable");
        std::fs::write(tmp.path().join("markdown._preinstalled.udl.xml"), &markdown)
            .expect("write UDL");
        std::fs::write(
            tmp.path().join("looks-like-xml.xml"),
            "<not-a-udl></not-a-udl>",
        )
        .expect("write decoy");

        let reg = UdlRegistry::scan_dir(tmp.path());
        assert_eq!(reg.entries().len(), 1);
        assert_eq!(reg.entries()[0].definition.name, "Markdown (preinstalled)");
    }

    #[test]
    fn assigns_sequential_ids_starting_at_base() {
        // Alphabetical sort → deterministic id assignment.
        let tmp = tempfile::tempdir().expect("tempdir must succeed");
        let markdown = std::fs::read_to_string(markdown_fixture_path())
            .expect("markdown fixture must be readable");
        // File names picked so the alphabetical order matches
        // what the scanner asserts.
        for name in ["aaa.udl.xml", "bbb.udl.xml", "ccc.udl.xml"] {
            std::fs::write(tmp.path().join(name), &markdown).expect("write UDL");
        }

        let reg = UdlRegistry::scan_dir(tmp.path());
        assert_eq!(reg.entries().len(), 3);
        assert_eq!(reg.entries()[0].lang_type_id, UDL_LANG_TYPE_BASE);
        assert_eq!(reg.entries()[1].lang_type_id, UDL_LANG_TYPE_BASE + 1);
        assert_eq!(reg.entries()[2].lang_type_id, UDL_LANG_TYPE_BASE + 2);
    }

    #[test]
    fn malformed_udl_skipped_but_others_load() {
        // Robustness invariant: one bad file doesn't take down
        // the whole scan.
        let tmp = tempfile::tempdir().expect("tempdir must succeed");
        let markdown = std::fs::read_to_string(markdown_fixture_path())
            .expect("markdown fixture must be readable");
        std::fs::write(tmp.path().join("a-good.udl.xml"), &markdown).expect("write good UDL");
        std::fs::write(
            tmp.path().join("b-broken.udl.xml"),
            "<NotepadPlus><UserLang>unterminated",
        )
        .expect("write broken UDL");

        let reg = UdlRegistry::scan_dir(tmp.path());
        assert_eq!(reg.entries().len(), 1);
        assert_eq!(reg.entries()[0].definition.name, "Markdown (preinstalled)");
    }

    #[test]
    fn find_by_lang_type_id_and_extension() {
        let tmp = tempfile::tempdir().expect("tempdir must succeed");
        let markdown = std::fs::read_to_string(markdown_fixture_path())
            .expect("markdown fixture must be readable");
        std::fs::write(tmp.path().join("markdown._preinstalled.udl.xml"), &markdown)
            .expect("write UDL");

        let reg = UdlRegistry::scan_dir(tmp.path());
        // Present id round-trips.
        let entry = reg
            .find_by_lang_type_id(UDL_LANG_TYPE_BASE)
            .expect("markdown UDL must resolve at base id");
        assert_eq!(entry.definition.name, "Markdown (preinstalled)");

        // Absent id → None (both out-of-range and empty-slot).
        assert!(reg.find_by_lang_type_id(0).is_none());
        assert!(reg.find_by_lang_type_id(UDL_LANG_TYPE_END).is_none());

        // Case-insensitive extension lookup — fixture's `ext`
        // attribute is `"md markdown"`.
        assert_eq!(
            reg.find_by_extension("md").map(|e| e.lang_type_id),
            Some(UDL_LANG_TYPE_BASE)
        );
        assert_eq!(
            reg.find_by_extension("MARKDOWN").map(|e| e.lang_type_id),
            Some(UDL_LANG_TYPE_BASE)
        );
        assert!(reg.find_by_extension("cpp").is_none());
    }

    #[test]
    fn extension_matcher_is_case_insensitive_on_filename() {
        // The scanner accepts `*.UDL.XML` too — some Git-Windows
        // installs mangle case, and a user hand-renaming files
        // shouldn't have to know which case matters.
        let tmp = tempfile::tempdir().expect("tempdir must succeed");
        let markdown = std::fs::read_to_string(markdown_fixture_path())
            .expect("markdown fixture must be readable");
        std::fs::write(tmp.path().join("EXAMPLE.UDL.XML"), &markdown)
            .expect("write uppercase-suffix UDL");

        let reg = UdlRegistry::scan_dir(tmp.path());
        assert_eq!(reg.entries().len(), 1);
    }

    #[test]
    fn dynamic_id_space_size_is_1024() {
        // Documented span pinned — a future rebalance of the ID
        // window must update this test and its dependents.
        assert_eq!(UDL_LANG_TYPE_END - UDL_LANG_TYPE_BASE + 1, 1024);
    }
}
