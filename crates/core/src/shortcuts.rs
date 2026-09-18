//! Plugin-command shortcut cache — Notepad++'s `shortcuts.xml` shape.
//!
//! A plugin's menu shortcuts arrive through `FuncItem._pShKey`, which
//! only exists after `getFuncsArray` — i.e. after the plugin is
//! loaded. Under lazy loading (DESIGN.md §6.4) that is a circularity:
//! a hotkey cannot trigger the load that would have taught the host
//! the hotkey. Notepad++ escapes it by caching plugin shortcuts in
//! `shortcuts.xml` across sessions; this module is Code++'s version
//! of that file.
//!
//! Shape (the `<PluginCommands>` section of N++'s own file, same
//! attribute spellings, so a migrating user can copy their file over):
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <NotepadPlus>
//!   <PluginCommands>
//!     <PluginCommand moduleName="mimeTools.dll" internalID="3"
//!                    Ctrl="yes" Alt="no" Shift="no" Key="66"/>
//!   </PluginCommands>
//! </NotepadPlus>
//! ```
//!
//! * `moduleName` — the plugin's file name on disk (`X.dll` /
//!   `libX.so` / `libX.dylib`). Matching is by [`module_key`] —
//!   lowercased, plugin extension and `lib` prefix stripped — so one
//!   file works across platforms and across NTFS case-insensitivity.
//! * `internalID` — the command's **index in the plugin's `FuncItem`
//!   array** (getFuncsArray order). The runtime `cmd_id` is assigned
//!   by load order and is not stable across sessions, so it is never
//!   persisted.
//! * `Key` — a Win32 virtual-key code, N++'s on-disk convention on
//!   every platform (plugin sources are written against `VK_*`).
//!   [`portable_key`] maps the common set to a platform-neutral form
//!   for the GTK / Cocoa backends.
//!
//! Semantics: this file holds the *effective* shortcut per command.
//! On plugin load the host inserts each `FuncItem` default that has
//! no entry yet; an existing entry always wins, which is what makes
//! a hand-edited remap stick across sessions. N++'s other sections
//! (`<Macros>`, `<ScintillaKeys>`, …) are tolerated on read and
//! **not** round-tripped: Code++ rewrites the file with only
//! `<PluginCommands>`. A user migrating a full N++ file should keep
//! their original — this file is Code++'s own persistence, not an
//! editor for N++'s.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::npp_session::YesNo;

/// Hard cap on the size of a `shortcuts.xml` this module will read.
/// The file is host-written but hand-editable (and copyable from an
/// N++ install), so the read is bounded the same way
/// [`crate::npp_session::MAX_SESSION_XML_BYTES`] bounds session
/// interchange: on the read itself, not on a TOCTOU-prone stat.
/// Even [`MAX_PLUGIN_SHORTCUTS`] maximal entries serialize to well
/// under 1 MiB.
pub const MAX_SHORTCUTS_XML_BYTES: u64 = 1024 * 1024;

/// Cap on retained entries after [`PluginShortcuts::clamp`]. Four
/// plugins ship in-tree and a heavy N++ install has a few dozen; a
/// thousand-entry file is already implausible, and the cap bounds
/// what a hostile hand-edit can make every keypress lookup walk.
pub const MAX_PLUGIN_SHORTCUTS: usize = 4096;

/// Highest `internalID` accepted by [`PluginShortcuts::clamp`].
/// Mirrors plugin-host's `MAX_FUNCITEMS` (1024) — an index at or
/// past the `FuncItem` cap can never resolve to a command.
pub const MAX_INTERNAL_ID: u32 = 1023;

/// Longest `moduleName` retained by [`PluginShortcuts::clamp`].
/// Plugin file names are path components; anything longer than this
/// is not a name Code++'s own discovery could have produced.
pub const MAX_MODULE_NAME_CHARS: usize = 256;

/// One persisted plugin-command shortcut — the runtime model the
/// rest of the workspace consumes (the serde structs below are the
/// wire shape only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginShortcut {
    /// Plugin file name as discovered on disk (`mimeTools.dll`,
    /// `libexample_hello.so`, …). Compare via [`module_key`], never
    /// byte-for-byte.
    pub module: String,
    /// Index into the plugin's `FuncItem` array.
    pub internal_id: u32,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Win32 virtual-key code. Never 0 after [`PluginShortcuts::clamp`].
    pub key: u8,
}

impl PluginShortcut {
    /// The normalized identity key for this entry's plugin.
    #[must_use]
    pub fn module_key(&self) -> String {
        module_key(&self.module)
    }

    /// Chord identity — two entries with equal tuples would race for
    /// the same keypress.
    #[must_use]
    pub const fn chord(&self) -> (bool, bool, bool, u8) {
        (self.ctrl, self.alt, self.shift, self.key)
    }

    /// Human-readable chord text ("Ctrl+Alt+F5"), platform-neutral
    /// spelling. The Cocoa backend substitutes its own glyphs; Win32
    /// and GTK append this verbatim after a `\t` in menu labels.
    #[must_use]
    pub fn display_label(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl+");
        }
        if self.alt {
            out.push_str("Alt+");
        }
        if self.shift {
            out.push_str("Shift+");
        }
        out.push_str(&vk_display_name(self.key));
        out
    }
}

/// Normalize a plugin file name to its cross-platform identity:
/// lowercase, one trailing plugin extension (`.dll` / `.so` /
/// `.dylib`) stripped, and — for the Unix extensions only — a
/// leading `lib` prefix stripped too. `mimeTools.dll`,
/// `MIMETOOLS.DLL` and `libmimetools.so` all yield `mimetools`, so
/// a shortcuts.xml written on one platform keys the same commands
/// on another (the same plugin *source* recompiles per platform —
/// DESIGN.md §6.1).
///
/// The `lib` strip is deliberately **`.so`/`.dylib`-only**: those
/// are Cargo's `cdylib` output prefix, an artifact to undo. Windows
/// never prepends `lib` to a `.dll`, so stripping it there would
/// corrupt a real name (`library.dll` → `rary`) and, worse, could
/// alias two unrelated plugins to one identity (`libFoo.dll` and
/// `Foo.dll` both → `foo`) — and every consumer keys on this string
/// alone. A bare extensionless name is returned lowercased verbatim.
#[must_use]
pub fn module_key(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for ext in [".dll", ".so", ".dylib"] {
        if let Some(stem) = lower.strip_suffix(ext) {
            return if ext == ".dll" {
                stem.to_string()
            } else {
                stem.strip_prefix("lib").unwrap_or(stem).to_string()
            };
        }
    }
    lower
}

/// Platform-neutral identity for the subset of virtual-key codes the
/// non-Windows backends can register. Win32 consumes the raw VK code
/// and ignores this mapping; GTK and Cocoa translate it to a keyval /
/// key-equivalent and skip (with a warning) chords that return `None`
/// from [`portable_key`] — layout-dependent OEM punctuation keys,
/// chiefly, which have no faithful cross-platform spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortableKey {
    /// A lowercase ASCII letter, `b'a'..=b'z'`.
    Letter(u8),
    /// An ASCII digit, `b'0'..=b'9'`.
    Digit(u8),
    /// A function key, 1..=24.
    Function(u8),
    Named(NamedKey),
}

/// Non-printing keys with a stable identity on all three platforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKey {
    Space,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
}

/// Map a Win32 virtual-key code to its [`PortableKey`], or `None`
/// for codes with no faithful cross-platform identity.
#[must_use]
pub fn portable_key(vk: u8) -> Option<PortableKey> {
    match vk {
        0x41..=0x5A => Some(PortableKey::Letter(vk - 0x41 + b'a')),
        0x30..=0x39 => Some(PortableKey::Digit(vk - 0x30 + b'0')),
        0x70..=0x87 => Some(PortableKey::Function(vk - 0x70 + 1)),
        0x20 => Some(PortableKey::Named(NamedKey::Space)),
        0x2D => Some(PortableKey::Named(NamedKey::Insert)),
        0x2E => Some(PortableKey::Named(NamedKey::Delete)),
        0x24 => Some(PortableKey::Named(NamedKey::Home)),
        0x23 => Some(PortableKey::Named(NamedKey::End)),
        0x21 => Some(PortableKey::Named(NamedKey::PageUp)),
        0x22 => Some(PortableKey::Named(NamedKey::PageDown)),
        0x25 => Some(PortableKey::Named(NamedKey::Left)),
        0x26 => Some(PortableKey::Named(NamedKey::Up)),
        0x27 => Some(PortableKey::Named(NamedKey::Right)),
        0x28 => Some(PortableKey::Named(NamedKey::Down)),
        _ => None,
    }
}

/// Display text for a virtual-key code. Falls back to the hex code
/// for keys [`portable_key`] cannot name — honest rather than
/// guessing at a layout-dependent glyph.
#[must_use]
pub fn vk_display_name(vk: u8) -> String {
    match portable_key(vk) {
        Some(PortableKey::Letter(c)) => char::from(c.to_ascii_uppercase()).to_string(),
        Some(PortableKey::Digit(c)) => char::from(c).to_string(),
        Some(PortableKey::Function(n)) => format!("F{n}"),
        Some(PortableKey::Named(n)) => match n {
            NamedKey::Space => "Space",
            NamedKey::Insert => "Ins",
            NamedKey::Delete => "Del",
            NamedKey::Home => "Home",
            NamedKey::End => "End",
            NamedKey::PageUp => "PgUp",
            NamedKey::PageDown => "PgDn",
            NamedKey::Left => "Left",
            NamedKey::Right => "Right",
            NamedKey::Up => "Up",
            NamedKey::Down => "Down",
        }
        .to_string(),
        None => format!("0x{vk:02X}"),
    }
}

/// Whether a chord is safe to install as an application accelerator.
///
/// Two rules, both about not stealing the keyboard from the editor:
///
/// * a chord must carry Ctrl or Alt, **except** function keys, which
///   are registrable bare or Shift-only (`F5`, `Shift+F3` are normal
///   plugin bindings). A bare or Shift-only letter / digit / nav key
///   would swallow ordinary typing and navigation, so a hand-edited
///   `Key="65"` with no modifiers is refused rather than honoured;
/// * [`is_reserved_editor_chord`] chords are refused outright.
#[must_use]
pub fn is_registrable_chord(ctrl: bool, alt: bool, shift: bool, key: u8) -> bool {
    if key == 0 || is_reserved_editor_chord(ctrl, alt, shift, key) {
        return false;
    }
    if matches!(portable_key(key), Some(PortableKey::Function(_))) {
        return true;
    }
    ctrl || alt
}

/// The plain-Ctrl chords Scintilla itself implements — clipboard,
/// undo/redo, select-all. Registering one as an accelerator would
/// intercept it before the editor sees it (on Win32 this is the
/// documented "Ctrl+V sometimes doesn't paste" failure the pump's
/// accelerator table deliberately omits these for), so a plugin —
/// or a hand-edited cache — asking for one is refused on every
/// backend. Modified variants (`Ctrl+Shift+Z`, `Ctrl+Alt+C`) do not
/// collide with Scintilla's defaults and stay allowed.
#[must_use]
pub fn is_reserved_editor_chord(ctrl: bool, alt: bool, shift: bool, key: u8) -> bool {
    // X, C, V, Z, Y, A
    ctrl && !alt && !shift && matches!(key, 0x58 | 0x43 | 0x56 | 0x5A | 0x59 | 0x41)
}

// --- wire shape ---------------------------------------------------------

/// `<PluginCommand>` element — N++'s attribute spellings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PluginCommandXml {
    #[serde(rename = "@moduleName", default)]
    module_name: String,
    #[serde(rename = "@internalID", default)]
    internal_id: u32,
    #[serde(rename = "@Ctrl", default)]
    ctrl: YesNo,
    #[serde(rename = "@Alt", default)]
    alt: YesNo,
    #[serde(rename = "@Shift", default)]
    shift: YesNo,
    #[serde(rename = "@Key", default)]
    key: u8,
}

/// `<PluginCommands>` container.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PluginCommandsXml {
    #[serde(rename = "PluginCommand", default)]
    items: Vec<PluginCommandXml>,
}

/// Root `<NotepadPlus>` element. N++'s other sections
/// (`<InternalCommands>`, `<Macros>`, `<UserDefinedCommands>`,
/// `<ScintillaKeys>`) are unknown fields to serde and are ignored on
/// parse — see the module docs for the (deliberate) non-round-trip.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "NotepadPlus")]
struct ShortcutsDocXml {
    #[serde(rename = "PluginCommands", default)]
    plugin_commands: PluginCommandsXml,
}

// --- store --------------------------------------------------------------

/// Errors from reading / writing `shortcuts.xml`.
#[derive(Debug)]
pub enum ShortcutsError {
    /// I/O failure on read/write.
    Io(std::io::Error),
    /// XML did not match the expected shape.
    Parse(quick_xml::DeError),
    /// Serialisation to XML failed.
    Serialize(quick_xml::SeError),
    /// The file exceeds [`MAX_SHORTCUTS_XML_BYTES`] and was refused
    /// before being parsed.
    TooLarge {
        /// The byte cap that was exceeded.
        limit: u64,
    },
}

impl std::fmt::Display for ShortcutsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShortcutsError::Io(e) => write!(f, "shortcuts I/O error: {e}"),
            ShortcutsError::Parse(e) => write!(f, "shortcuts parse error: {e}"),
            ShortcutsError::Serialize(e) => write!(f, "shortcuts serialize error: {e}"),
            ShortcutsError::TooLarge { limit } => {
                write!(f, "shortcuts file exceeds the {limit}-byte size limit")
            }
        }
    }
}

impl std::error::Error for ShortcutsError {}

impl From<std::io::Error> for ShortcutsError {
    fn from(e: std::io::Error) -> Self {
        ShortcutsError::Io(e)
    }
}

/// The persisted set of plugin-command shortcuts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginShortcuts {
    entries: Vec<PluginShortcut>,
}

impl PluginShortcuts {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    #[must_use]
    pub fn entries(&self) -> &[PluginShortcut] {
        &self.entries
    }

    /// Look up the entry for `(module_key, internal_id)`.
    #[must_use]
    pub fn get(&self, module_key_norm: &str, internal_id: u32) -> Option<&PluginShortcut> {
        self.entries
            .iter()
            .find(|e| e.internal_id == internal_id && e.module_key() == module_key_norm)
    }

    /// Insert `entry` unless an entry for the same command already
    /// exists — the existing entry wins, which is what makes a
    /// user's hand-edited remap outlive the plugin's own default.
    /// Returns whether the set changed.
    pub fn insert_default(&mut self, entry: PluginShortcut) -> bool {
        if self.entries.len() >= MAX_PLUGIN_SHORTCUTS {
            return false;
        }
        if self.get(&entry.module_key(), entry.internal_id).is_some() {
            return false;
        }
        self.entries.push(entry);
        true
    }

    /// Remove the entry for `(module_key, internal_id)`. Returns
    /// whether an entry existed.
    pub fn remove(&mut self, module_key_norm: &str, internal_id: u32) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|e| !(e.internal_id == internal_id && e.module_key() == module_key_norm));
        self.entries.len() != before
    }

    /// Second-layer defence after parse (same discipline as
    /// `Styles::clamp`): drop entries no discovery could have
    /// produced (`Key="0"`, out-of-range `internalID`, absurd
    /// module names), dedupe by command keeping the first, and cap
    /// the total. Chord *collisions between commands* are legal
    /// here — which one wins a keypress is resolved at registration
    /// time, where the discovered-plugin set is known.
    ///
    /// Dedup is `HashSet`-based (O(n)), not a linear `Vec` scan: a
    /// hand-edited file just under [`MAX_SHORTCUTS_XML_BYTES`] holds
    /// tens of thousands of entries *before* the count cap applies,
    /// and an O(n²) dedup over that pre-cap set would freeze the
    /// startup path (`Shell::new` parses this synchronously). The
    /// count cap ([`MAX_PLUGIN_SHORTCUTS`]) bounds storage; this
    /// bounds the cost of getting there.
    pub fn clamp(&mut self) {
        self.entries.retain(|e| {
            e.key != 0
                && e.internal_id <= MAX_INTERNAL_ID
                && !e.module.is_empty()
                && e.module.chars().count() <= MAX_MODULE_NAME_CHARS
        });
        let mut seen: std::collections::HashSet<(String, u32)> = std::collections::HashSet::new();
        self.entries
            .retain(|e| seen.insert((e.module_key(), e.internal_id)));
        self.entries.truncate(MAX_PLUGIN_SHORTCUTS);
    }

    /// Read from `path`. A missing file is first-run and yields the
    /// empty set (this is host-owned persistence, like `styles.xml`
    /// — not user-picked interchange like `npp_session`).
    ///
    /// # Errors
    ///
    /// `Io` on read failure, `Parse` on malformed XML, `TooLarge`
    /// past [`MAX_SHORTCUTS_XML_BYTES`].
    pub fn load_from_xml(path: &Path) -> Result<Self, ShortcutsError> {
        use std::io::Read;
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(e) => return Err(ShortcutsError::Io(e)),
        };
        // Bounded read on the read itself, not a stat — the
        // npp_session.rs rationale applies verbatim.
        let mut buf = Vec::new();
        file.take(MAX_SHORTCUTS_XML_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(ShortcutsError::Io)?;
        if buf.len() as u64 > MAX_SHORTCUTS_XML_BYTES {
            return Err(ShortcutsError::TooLarge {
                limit: MAX_SHORTCUTS_XML_BYTES,
            });
        }
        let contents = String::from_utf8(buf).map_err(|e| {
            ShortcutsError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        Self::from_xml_str(&contents)
    }

    /// Parse from an in-memory string. Runs [`Self::clamp`] on the
    /// result, so a loaded set is always within bounds.
    ///
    /// # Errors
    ///
    /// `Parse` when the bytes don't deserialise.
    pub fn from_xml_str(s: &str) -> Result<Self, ShortcutsError> {
        let doc: ShortcutsDocXml = quick_xml::de::from_str(s).map_err(ShortcutsError::Parse)?;
        let mut out = Self {
            entries: doc
                .plugin_commands
                .items
                .into_iter()
                .map(|i| PluginShortcut {
                    module: i.module_name,
                    internal_id: i.internal_id,
                    ctrl: i.ctrl.as_bool(),
                    alt: i.alt.as_bool(),
                    shift: i.shift.as_bool(),
                    key: i.key,
                })
                .collect(),
        };
        out.clamp();
        Ok(out)
    }

    /// Write to `path` atomically via temp-file + rename — the
    /// `Session::save_to_xml` discipline.
    ///
    /// # Errors
    ///
    /// `Serialize` if quick-xml refuses the document; `Io` on
    /// directory creation, write, sync, or rename.
    pub fn save_to_xml(&self, path: &Path) -> Result<(), ShortcutsError> {
        let xml = self.to_xml_string()?;

        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent)?;
        }
        let parent_dir = parent.unwrap_or_else(|| Path::new("."));

        let mut tmp = tempfile::Builder::new()
            .prefix(".shortcuts-")
            .suffix(".xml.tmp")
            .tempfile_in(parent_dir)?;
        tmp.write_all(xml.as_bytes())?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(path).map_err(|e| ShortcutsError::Io(e.error))?;
        Ok(())
    }

    /// Serialise to an XML string with the standard prolog.
    ///
    /// # Errors
    ///
    /// `Serialize` when quick-xml refuses to emit (rare).
    pub fn to_xml_string(&self) -> Result<String, ShortcutsError> {
        let doc = ShortcutsDocXml {
            plugin_commands: PluginCommandsXml {
                items: self
                    .entries
                    .iter()
                    .map(|e| PluginCommandXml {
                        module_name: e.module.clone(),
                        internal_id: e.internal_id,
                        ctrl: YesNo::from_bool(e.ctrl),
                        alt: YesNo::from_bool(e.alt),
                        shift: YesNo::from_bool(e.shift),
                        key: e.key,
                    })
                    .collect(),
            },
        };
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        quick_xml::se::to_writer(&mut xml, &doc).map_err(ShortcutsError::Serialize)?;
        Ok(xml)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(module: &str, id: u32, ctrl: bool, alt: bool, shift: bool, key: u8) -> PluginShortcut {
        PluginShortcut {
            module: module.into(),
            internal_id: id,
            ctrl,
            alt,
            shift,
            key,
        }
    }

    #[test]
    fn round_trips_entries() {
        let mut store = PluginShortcuts::new();
        assert!(store.insert_default(entry("mimeTools.dll", 3, true, false, false, 0x42)));
        assert!(store.insert_default(entry("libexample_hello.so", 0, true, true, false, 0x48)));
        let xml = store.to_xml_string().unwrap();
        let parsed = PluginShortcuts::from_xml_str(&xml).unwrap();
        assert_eq!(store, parsed);
    }

    #[test]
    fn wire_format_matches_npp_spellings() {
        let mut store = PluginShortcuts::new();
        store.insert_default(entry("mimeTools.dll", 3, true, false, true, 0x42));
        let xml = store.to_xml_string().unwrap();
        assert!(xml.contains(r#"moduleName="mimeTools.dll""#), "got: {xml}");
        assert!(xml.contains(r#"internalID="3""#), "got: {xml}");
        assert!(xml.contains(r#"Ctrl="yes""#), "got: {xml}");
        assert!(xml.contains(r#"Alt="no""#), "got: {xml}");
        assert!(xml.contains(r#"Shift="yes""#), "got: {xml}");
        assert!(xml.contains(r#"Key="66""#), "got: {xml}");
        assert!(xml.contains("<PluginCommands>"), "got: {xml}");
    }

    /// A real-shaped N++ shortcuts.xml — full sibling-section cloud —
    /// parses, and only `<PluginCommands>` contributes.
    #[test]
    fn parses_full_npp_wire_shape() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<NotepadPlus>
    <InternalCommands />
    <Macros>
        <Macro name="Trim" Ctrl="no" Alt="no" Shift="no" Key="0">
            <Action type="2" message="0" wParam="42024" lParam="0" sParam="" />
        </Macro>
    </Macros>
    <UserDefinedCommands>
        <Command name="Get php help" Ctrl="no" Alt="yes" Shift="no" Key="112">https://www.php.net</Command>
    </UserDefinedCommands>
    <PluginCommands>
        <PluginCommand moduleName="mimeTools.dll" internalID="4" Ctrl="yes" Alt="no" Shift="no" Key="66" />
        <PluginCommand moduleName="NppExec.dll" internalID="0" Ctrl="no" Alt="no" Shift="no" Key="117" />
    </PluginCommands>
    <ScintillaKeys />
</NotepadPlus>"#;
        let store = PluginShortcuts::from_xml_str(xml).unwrap();
        assert_eq!(store.entries().len(), 2);
        assert_eq!(store.entries()[0].module, "mimeTools.dll");
        assert_eq!(store.entries()[0].internal_id, 4);
        assert!(store.entries()[0].ctrl);
        assert_eq!(store.entries()[0].key, 0x42);
        // Bare F6 (NppExec's classic Execute binding) survives the
        // parse — registrability is a registration-time policy, not
        // a parse-time one.
        assert_eq!(store.entries()[1].key, 117);
        assert!(!store.entries()[1].ctrl);
    }

    #[test]
    fn missing_file_is_first_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = PluginShortcuts::load_from_xml(&dir.path().join("nope.xml")).unwrap();
        assert!(store.entries().is_empty());
    }

    #[test]
    fn save_and_load_via_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shortcuts.xml");
        let mut store = PluginShortcuts::new();
        store.insert_default(entry("a.dll", 1, true, false, false, 0x51));
        store.save_to_xml(&path).unwrap();
        assert_eq!(PluginShortcuts::load_from_xml(&path).unwrap(), store);
    }

    #[test]
    fn load_refuses_a_file_over_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.xml");
        let over_cap = usize::try_from(MAX_SHORTCUTS_XML_BYTES + 1).unwrap();
        let mut bytes = Vec::with_capacity(over_cap + 8);
        bytes.extend_from_slice(b"<!-- ");
        bytes.resize(over_cap, b'x');
        std::fs::write(&path, &bytes).unwrap();
        let err = PluginShortcuts::load_from_xml(&path).unwrap_err();
        assert!(
            matches!(err, ShortcutsError::TooLarge { limit } if limit == MAX_SHORTCUTS_XML_BYTES),
            "expected TooLarge, got {err:?}",
        );
    }

    #[test]
    fn clamp_drops_invalid_and_dedupes() {
        let xml = format!(
            r#"<NotepadPlus><PluginCommands>
            <PluginCommand moduleName="a.dll" internalID="0" Ctrl="yes" Key="65"/>
            <PluginCommand moduleName="A.DLL" internalID="0" Ctrl="yes" Key="66"/>
            <PluginCommand moduleName="a.dll" internalID="9999" Ctrl="yes" Key="65"/>
            <PluginCommand moduleName="a.dll" internalID="1" Ctrl="yes" Key="0"/>
            <PluginCommand moduleName="" internalID="2" Ctrl="yes" Key="65"/>
            <PluginCommand moduleName="{}" internalID="3" Ctrl="yes" Key="65"/>
            </PluginCommands></NotepadPlus>"#,
            "x".repeat(MAX_MODULE_NAME_CHARS + 1)
        );
        let store = PluginShortcuts::from_xml_str(&xml).unwrap();
        // Only the first a.dll/0 survives: the case-folded duplicate
        // loses, the out-of-range internalID / zero key / empty and
        // oversized module names are dropped.
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].key, 65);
        assert_eq!(store.entries()[0].module, "a.dll");
    }

    #[test]
    fn insert_default_never_overwrites() {
        let mut store = PluginShortcuts::new();
        assert!(store.insert_default(entry("a.dll", 0, true, false, false, 0x41)));
        // Same command, different chord, different module spelling —
        // the stored (possibly user-edited) entry wins.
        assert!(!store.insert_default(entry("libA.so", 0, true, true, false, 0x42)));
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].key, 0x41);
    }

    #[test]
    fn remove_is_by_normalized_module_key() {
        let mut store = PluginShortcuts::new();
        store.insert_default(entry("MimeTools.DLL", 4, true, false, false, 0x42));
        assert!(store.remove("mimetools", 4));
        assert!(!store.remove("mimetools", 4));
        assert!(store.entries().is_empty());
    }

    #[test]
    fn module_key_normalizes_across_platforms() {
        assert_eq!(module_key("mimeTools.dll"), "mimetools");
        assert_eq!(module_key("MIMETOOLS.DLL"), "mimetools");
        assert_eq!(module_key("libmimetools.so"), "mimetools");
        assert_eq!(module_key("libmimetools.dylib"), "mimetools");
        assert_eq!(module_key("example_hello.dll"), "example_hello");
        assert_eq!(module_key("libexample_hello.so"), "example_hello");
        // No plugin extension → lowercased verbatim, `lib` kept.
        assert_eq!(module_key("library"), "library");
        // Dotted names lose only the final plugin extension.
        assert_eq!(module_key("my.plugin.dll"), "my.plugin");
        // A `.dll` never had a `lib` artifact to strip — Windows does
        // not prepend one. Stripping it would corrupt real names and
        // could alias unrelated plugins, so `.dll` keeps its stem
        // verbatim while the Unix extensions strip `lib`.
        assert_eq!(module_key("library.dll"), "library");
        assert_eq!(module_key("libgit-helper.dll"), "libgit-helper");
        assert_eq!(module_key("Libation.dll"), "libation");
        // The aliasing hazard the .dll carve-out prevents: these two
        // unrelated plugins must NOT collapse to one identity.
        assert_ne!(module_key("libFoo.dll"), module_key("Foo.dll"));
        // The Unix strip is unchanged — `lib` there is Cargo's prefix.
        assert_eq!(module_key("libFoo.so"), module_key("Foo.so"));
    }

    #[test]
    fn portable_key_maps_the_common_set() {
        assert_eq!(portable_key(0x41), Some(PortableKey::Letter(b'a')));
        assert_eq!(portable_key(0x5A), Some(PortableKey::Letter(b'z')));
        assert_eq!(portable_key(0x30), Some(PortableKey::Digit(b'0')));
        assert_eq!(portable_key(0x70), Some(PortableKey::Function(1)));
        assert_eq!(portable_key(0x87), Some(PortableKey::Function(24)));
        assert_eq!(
            portable_key(0x2E),
            Some(PortableKey::Named(NamedKey::Delete))
        );
        // OEM punctuation is layout-dependent — declined, not guessed.
        assert_eq!(portable_key(0xBF), None);
        assert_eq!(portable_key(0), None);
    }

    #[test]
    fn display_labels() {
        assert_eq!(
            entry("a.dll", 0, true, true, false, 0x48).display_label(),
            "Ctrl+Alt+H"
        );
        assert_eq!(
            entry("a.dll", 0, false, false, true, 0x72).display_label(),
            "Shift+F3"
        );
        assert_eq!(
            entry("a.dll", 0, true, false, false, 0xBF).display_label(),
            "Ctrl+0xBF"
        );
    }

    #[test]
    fn registrability_policy() {
        // Ctrl / Alt chords are fine.
        assert!(is_registrable_chord(true, false, false, 0x48));
        assert!(is_registrable_chord(false, true, false, 0x48));
        // Bare or Shift-only letters / digits / nav keys would
        // swallow typing — refused.
        assert!(!is_registrable_chord(false, false, false, 0x48));
        assert!(!is_registrable_chord(false, false, true, 0x48));
        assert!(!is_registrable_chord(false, false, false, 0x2E));
        // Function keys are registrable bare and Shift-only.
        assert!(is_registrable_chord(false, false, false, 0x75));
        assert!(is_registrable_chord(false, false, true, 0x72));
        // Zero key never registers.
        assert!(!is_registrable_chord(true, false, false, 0));
        // Scintilla-native chords are refused…
        assert!(!is_registrable_chord(true, false, false, 0x56));
        assert!(!is_registrable_chord(true, false, false, 0x5A));
        // …but their modified variants are not Scintilla's defaults.
        assert!(is_registrable_chord(true, false, true, 0x5A));
        assert!(is_registrable_chord(true, true, false, 0x43));
    }
}
