//! The XML documents Code++ writes for its own files: `session.xml`,
//! `config.xml`, `styles.xml`, `shortcuts.xml`, the recent-files and
//! find-history lists, and Notepad++-shape session files.

/// Serialise `value` as a whole XML document: the declaration, then
/// quick-xml's serialisation of `value`, with every tab, line feed and
/// carriage return written as a character reference.
///
/// quick-xml writes those three characters as they are. An XML reader then
/// turns each one in an attribute into a space, and a carriage return in
/// text into a line feed, as XML 1.0 requires, so a path with a tab in it
/// came back naming a different file. Written as references they read back
/// as themselves; the UDL writer escapes its attributes the same way. The
/// serialisation puts no whitespace of its own between tags, since nothing
/// here indents it, so every such character in it is data.
///
/// The characters XML 1.0 does not allow at all, the other control
/// characters below U+0020, U+FFFE and U+FFFF, are written as they are:
/// no reference can stand for them either, so a document holding one is
/// not well-formed XML. quick-xml reads such a document back as written,
/// so Code++'s own files keep them; another XML tool may refuse the file.
pub(crate) fn to_document<T: serde::Serialize + ?Sized>(
    value: &T,
) -> Result<String, quick_xml::SeError> {
    let mut body = String::new();
    quick_xml::se::to_writer(&mut body, value)?;
    let mut xml = String::with_capacity(DECLARATION.len() + body.len());
    xml.push_str(DECLARATION);
    for c in body.chars() {
        match c {
            '\t' => xml.push_str("&#x9;"),
            '\n' => xml.push_str("&#xA;"),
            '\r' => xml.push_str("&#xD;"),
            c => xml.push(c),
        }
    }
    Ok(xml)
}

/// The first line of every document [`to_document`] writes.
const DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename = "doc")]
    struct Doc {
        #[serde(rename = "@attr")]
        attr: String,
        #[serde(rename = "item")]
        item: String,
    }

    /// Tabs and line breaks come back as themselves, in an attribute and in
    /// text alike, where quick-xml's own output brought them back as spaces
    /// and line feeds.
    #[test]
    fn whitespace_in_a_value_survives_a_round_trip() {
        let doc = Doc {
            attr: "a\tb\nc\rd\r\ne f".into(),
            item: "g\th\ni\rj\r\nk".into(),
        };
        let xml = to_document(&doc).unwrap();
        assert!(xml.starts_with(DECLARATION));
        assert!(
            !xml[DECLARATION.len()..].contains(['\t', '\n', '\r']),
            "{xml:?}"
        );
        let back: Doc = quick_xml::de::from_str(&xml).unwrap();
        assert_eq!(back, doc);
    }

    /// Why [`to_document`] writes references: quick-xml's own output brings
    /// tabs and line breaks back as spaces and line feeds. Should it come to
    /// keep them by itself, this fails, and the references are no longer
    /// needed; the round trip above passes either way.
    #[test]
    fn quick_xml_alone_loses_whitespace_in_a_value() {
        let doc = Doc {
            attr: "a\tb\nc\rd".into(),
            item: "e\rf".into(),
        };
        let mut raw = String::new();
        quick_xml::se::to_writer(&mut raw, &doc).unwrap();
        let lossy: Doc = quick_xml::de::from_str(&raw).unwrap();
        assert_eq!(
            lossy,
            Doc {
                attr: "a b c d".into(),
                item: "e\nf".into(),
            }
        );
    }

    /// The serialisation adds no whitespace of its own, which is what lets
    /// [`to_document`] write every tab and line break it finds as data: each
    /// document Code++ writes, holding none in its values, comes out with no
    /// reference to one. A serialisation that indented would put line feeds
    /// between the tags, and written as references they would read back as
    /// text.
    #[test]
    fn the_documents_written_add_no_whitespace_of_their_own() {
        use crate::dock::DockLayout;
        use crate::find_history::FindHistory;
        use crate::npp_session::{NppSession, NppSessionDoc, NppView};
        use crate::preferences::Preferences;
        use crate::recent_files::RecentFiles;
        use crate::session::{DocMapSession, Session, Tab, WorkspaceSession};
        use crate::shortcuts::{PluginShortcut, PluginShortcuts};
        use crate::styles::{StyleEntry, Styles, Transparency};

        let session = Session {
            active: Some(0),
            workspace: Some(WorkspaceSession {
                root: Some(PathBuf::from("/w")),
                ..WorkspaceSession::default()
            }),
            docmap: Some(DocMapSession::default()),
            dock: Some(DockLayout::new().to_session()),
            tabs: vec![
                Tab {
                    path: Some(PathBuf::from("/w/a.rs")),
                    ..Tab::default()
                },
                Tab::default(),
            ],
            ..Session::default()
        };
        let styles = Styles {
            default: Some(StyleEntry::default()),
            transparency: Some(Transparency::default()),
        };
        let mut recent = RecentFiles::default();
        recent.push(Path::new("/w/a.rs"));
        recent.push(Path::new("/w/b.rs"));
        let mut history = FindHistory::default();
        history.push_find("needle");
        history.push_replace("thread");
        let mut shortcuts = PluginShortcuts::new();
        shortcuts.insert_default(PluginShortcut {
            module: "a.dll".into(),
            internal_id: 1,
            ctrl: true,
            alt: false,
            shift: true,
            key: 0x41,
        });
        let npp = NppSessionDoc {
            session: NppSession {
                main_view: NppView {
                    files: vec![crate::npp_session::NppFile {
                        filename: "/w/a.rs".into(),
                        ..crate::npp_session::NppFile::default()
                    }],
                    ..NppView::default()
                },
                sub_view: Some(NppView::default()),
                ..NppSession::default()
            },
        };
        let documents = [
            ("session", to_document(&session).unwrap()),
            ("styles", to_document(&styles).unwrap()),
            ("config", to_document(&Preferences::default()).unwrap()),
            ("recent files", to_document(&recent).unwrap()),
            ("find history", to_document(&history).unwrap()),
            ("shortcuts", shortcuts.to_xml_string().unwrap()),
            ("Notepad++ session", npp.to_xml_string().unwrap()),
        ];
        assert!(documents[0].1.contains("<tab ") && documents[0].1.contains("<dock"));
        for (name, xml) in documents {
            assert!(
                !["&#x9;", "&#xA;", "&#xD;"].iter().any(|r| xml.contains(r)),
                "the {name} document holds whitespace of its own: {xml}"
            );
        }
    }

    /// The line of `source` that names quick-xml other than to read with
    /// it, if any: comment lines aside, every mention of the crate must be
    /// `quick_xml::de`, `quick_xml::DeError` or `quick_xml::SeError`, the
    /// last two only naming its errors. So an alias, a grouped import or
    /// `extern crate` is refused as well as a serialiser, since each can
    /// reach one; a reading use the list does not have, quick-xml's event
    /// reader say, is added to it.
    fn serialises_xml(source: &str) -> Option<&str> {
        const READING: [&str; 3] = ["de", "DeError", "SeError"];
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        for line in source.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for (at, name) in line.match_indices("quick_xml") {
                let before = line[..at].chars().next_back();
                let after = &line[at + name.len()..];
                if before.is_some_and(is_ident) || after.starts_with(is_ident) {
                    continue;
                }
                let item = after
                    .strip_prefix("::")
                    .map(|rest| rest.split(|c: char| !is_ident(c)).next().unwrap_or(""));
                if !item.is_some_and(|item| READING.contains(&item)) {
                    return Some(line.trim());
                }
            }
        }
        None
    }

    #[test]
    fn the_scan_finds_what_it_bans() {
        for banned in [
            "quick_xml::se::to_string(x)",
            "let w = quick_xml::Writer::new(out);",
            "use quick_xml::{de::from_str, se};",
            "use quick_xml::{events::{BytesEnd, Event}, se::to_string};",
            "use quick_xml::{de::{Deserializer, from_str}, se};",
            "use quick_xml as qx;",
            "extern crate quick_xml as q;",
            "Err(quick_xml::SeError) // then quick_xml::se::to_string(x)",
        ] {
            assert_eq!(serialises_xml(banned), Some(banned), "{banned}");
        }
        for allowed in [
            "Err(quick_xml::SeError)",
            "Parse(quick_xml::DeError),",
            "quick_xml::de::from_str(s)",
            "use quick_xml::de::from_str;",
            "    /// `quick_xml::se` would lose the tabs",
            "serde_json::Serializer::new(out)",
            "let my_quick_xml_writer = 1;",
            "use quick_xml_ext::thing;",
            "use not_quick_xml::thing;",
        ] {
            assert_eq!(serialises_xml(allowed), None, "{allowed}");
        }
    }

    /// Whether `manifest` renames quick-xml, which would let a source reach
    /// its serialiser under a name [`serialises_xml`] does not know.
    fn renames_quick_xml(manifest: &str) -> bool {
        manifest.lines().any(|line| {
            let code: String = line
                .split('#')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .collect();
            code.contains("package=\"quick-xml\"") || code.contains("package='quick-xml'")
        })
    }

    #[test]
    fn the_manifest_check_finds_a_rename() {
        for renamed in [
            "qx = { package = \"quick-xml\", version = \"0.41\" }",
            "[dependencies.qx]\npackage = 'quick-xml'",
        ] {
            assert!(renames_quick_xml(renamed), "{renamed}");
        }
        for plain in [
            "quick-xml = { workspace = true }",
            "quick-xml = { version = \"0.41\", features = [\"serialize\"] }",
            "# qx = { package = \"quick-xml\" }",
        ] {
            assert!(!renames_quick_xml(plain), "{plain}");
        }
    }

    /// No Rust source in the workspace but this one serialises with
    /// quick-xml, which writes tabs and line breaks as they are, so every
    /// document written that way goes through [`to_document`], and no
    /// manifest renames the crate. The UDL writer builds its XML by hand
    /// and writes its own references.
    #[test]
    fn nothing_else_in_the_workspace_serialises_xml() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut manifests = Vec::new();
        collect_files(&root, is_manifest, &mut manifests);
        assert!(
            manifests.contains(&root.join("Cargo.toml"))
                && manifests.contains(&root.join("crates/core/Cargo.toml")),
            "the walk did not find the manifests"
        );
        for manifest in &manifests {
            let text = String::from_utf8_lossy(&std::fs::read(manifest).unwrap()).into_owned();
            assert!(
                !renames_quick_xml(&text),
                "{} renames quick-xml",
                manifest.display()
            );
        }
        let mut files = Vec::new();
        collect_files(&root, is_rust_source, &mut files);
        let here = root.join("crates/core/src/xml.rs");
        for (what, found) in [
            ("this file", files.contains(&here)),
            (
                "the plugins",
                files.iter().any(|f| f.starts_with(root.join("plugins"))),
            ),
            (
                "another crate",
                files
                    .iter()
                    .any(|f| f.starts_with(root.join("crates/shell"))),
            ),
        ] {
            assert!(found, "the walk did not find {what}");
        }
        for file in files.iter().filter(|f| **f != here) {
            let source = String::from_utf8_lossy(&std::fs::read(file).unwrap()).into_owned();
            if let Some(line) = serialises_xml(&source) {
                panic!(
                    "{} names quick-xml other than to read with it: {line}",
                    file.display()
                );
            }
        }
    }

    /// Every file under `dir` that `wanted` picks, skipping build output,
    /// vendored sources and hidden folders. A folder that cannot be read
    /// fails the test rather than leaving its files unchecked.
    fn collect_files(dir: &Path, wanted: fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "target" || name == "vendor" || name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, wanted, out);
            } else if wanted(&path) {
                out.push(path);
            }
        }
    }

    fn is_rust_source(path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "rs")
    }

    fn is_manifest(path: &Path) -> bool {
        path.file_name().is_some_and(|n| n == "Cargo.toml")
    }

    /// The walk fails on a folder it cannot read rather than passing over
    /// it. A file stands in for the folder, since reading one as a folder
    /// fails for every account, root's included.
    #[test]
    fn the_walk_fails_on_a_folder_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-folder.rs");
        std::fs::write(&file, "").unwrap();
        let walked = std::panic::catch_unwind(|| {
            let mut out = Vec::new();
            collect_files(&file, is_rust_source, &mut out);
        });
        assert!(walked.is_err());
    }
}
