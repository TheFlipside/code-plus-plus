//! Editor-style configuration model + `styles.xml` round-trip.
//!
//! Persistent companion to `session.xml`: where `session.xml` carries
//! the user's open tabs and window state, `styles.xml` carries the
//! visual configuration the Style Configurator dialog edits — font
//! face, size, bold / italic / underline, foreground / background
//! colour, and transparency. Both files live under
//! `platform::config_dir()`.
//!
//! The scope today is the **Default Style** only (the
//! `STYLE_DEFAULT` Scintilla style index that `SCI_STYLECLEARALL`
//! propagates to every other style). Per-language style overrides
//! and theme files (`stylers.xml` shape from Notepad++) are
//! Phase 4.5 / Phase 5 work; this module's schema is forward-
//! compatible with adding more `<style>` rows later.
//!
//! Schema (stable from this point onward):
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <styles>
//!   <default font_name="Courier New" font_size="10"
//!            bold="false" italic="false" underline="false"
//!            fg="000000" bg="FFFFFF"/>
//!   <transparency enabled="false" percent="80"/>
//! </styles>
//! ```
//!
//! Colours are serialised as 6-hex-digit `RRGGBB` strings (not the
//! Win32 `0xBBGGRR` `COLORREF` byte order) so the persisted file
//! reads correctly when opened in a browser / colour picker. The
//! conversion to `COLORREF` happens at the Win32 boundary.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One style row (today: only the `<default>` entry — future
/// per-language style rows reuse this shape).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleEntry {
    /// Font face name as the user picks it from the system font
    /// dropdown. Stored verbatim — no normalisation. Empty string
    /// means "fall back to the platform default monospace face",
    /// which the renderer resolves via its own `Default` impl.
    #[serde(rename = "@font_name", default = "default_font_name")]
    pub font_name: String,
    /// Point size. `u16` since Win32 / Scintilla cap font size
    /// well below `u16::MAX`; smaller type also keeps the XML
    /// smaller and the field always positive.
    #[serde(rename = "@font_size", default = "default_font_size")]
    pub font_size: u16,
    #[serde(rename = "@bold", default)]
    pub bold: bool,
    #[serde(rename = "@italic", default)]
    pub italic: bool,
    #[serde(rename = "@underline", default)]
    pub underline: bool,
    /// Foreground colour as `RRGGBB` hex. Defaults to `000000`
    /// (black). The persisted form intentionally omits the leading
    /// `#` and uses upper-case so a `git diff` is stable.
    #[serde(rename = "@fg", default = "default_fg")]
    pub fg: String,
    /// Background colour as `RRGGBB` hex. Defaults to `FFFFFF`.
    #[serde(rename = "@bg", default = "default_bg")]
    pub bg: String,
}

impl Default for StyleEntry {
    fn default() -> Self {
        Self {
            font_name: default_font_name(),
            font_size: default_font_size(),
            bold: false,
            italic: false,
            underline: false,
            fg: default_fg(),
            bg: default_bg(),
        }
    }
}

fn default_font_name() -> String {
    "Courier New".to_string()
}

fn default_font_size() -> u16 {
    10
}

fn default_fg() -> String {
    "000000".to_string()
}

fn default_bg() -> String {
    "FFFFFF".to_string()
}

/// Transparency settings — visible-only-when-enabled, so the
/// `enabled` flag carries the user's intent independently of the
/// `percent` slider's last-set value. Skipping serialisation when
/// equal to the all-default value keeps `styles.xml` minimal for
/// users who haven't touched transparency.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transparency {
    #[serde(rename = "@enabled", default)]
    pub enabled: bool,
    /// Opacity percent in the inclusive range 20..=100. Lower
    /// values reach quickly into "you can't read the editor"
    /// territory; the dialog's slider enforces the same floor.
    /// 100 means fully opaque.
    #[serde(rename = "@percent", default = "default_transparency_percent")]
    pub percent: u8,
}

impl Default for Transparency {
    fn default() -> Self {
        Self {
            enabled: false,
            percent: default_transparency_percent(),
        }
    }
}

fn default_transparency_percent() -> u8 {
    80
}

/// Top-level `<styles>` document. The Style Configurator dialog
/// reads and writes one of these via [`save_to_xml`] /
/// [`load_from_xml`]; the `shell` crate caches the latest value
/// and the UI applies it through `UiPlatform::apply_default_style`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "styles")]
pub struct Styles {
    /// The single default-style row. Optional only so a malformed
    /// `<styles>` document with the row missing still parses
    /// cleanly (the loader falls back to `StyleEntry::default()`).
    #[serde(rename = "default", default, skip_serializing_if = "Option::is_none")]
    pub default: Option<StyleEntry>,
    /// Transparency settings, optional so older `styles.xml` files
    /// without the section round-trip cleanly.
    #[serde(
        rename = "transparency",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub transparency: Option<Transparency>,
}

/// Hard cap on `styles.xml` file size before we attempt to parse.
/// The schema is small (a `<default>` row + a `<transparency>`
/// row, each with a handful of attributes), so any healthy file
/// is well under 1 KiB. Capping at 4 KiB still leaves an order-
/// of-magnitude headroom for future schema growth without
/// inviting a DoS via a maliciously-large pre-planted file.
/// Hitting the cap surfaces as a `StylesError::Io` which
/// `load_styles` in the shell maps to defaults — same fallback
/// as a parse error.
pub const STYLES_XML_MAX_BYTES: u64 = 4096;

/// Maximum accepted length for `StyleEntry.font_name` after
/// deserialisation, measured in **Unicode scalar values** (Rust
/// `char`s). Real-world font face names are well under 64 chars
/// even for CJK faces; capping at 256 still leaves headroom for
/// any reasonable post-script name without inviting a multi-
/// megabyte allocation from a hand-crafted `styles.xml`.
pub const FONT_NAME_MAX_LEN: usize = 256;

/// Truncate `name` in place to at most [`FONT_NAME_MAX_LEN`]
/// chars (not bytes). Unicode-safe: never splits a multi-byte
/// codepoint. `String::truncate` operates at byte offsets and
/// panics if the offset lands mid-codepoint, which a CJK font
/// name would trigger if the byte truncation were naively at
/// `FONT_NAME_MAX_LEN`. This helper finds the byte offset of
/// the (`FONT_NAME_MAX_LEN`+1)-th char and truncates there, so
/// the result is always valid UTF-8 with length ≤ the cap.
fn truncate_font_name(name: &mut String) {
    let mut indices = name.char_indices();
    let cap_byte = match indices.nth(FONT_NAME_MAX_LEN) {
        Some((idx, _)) => idx,
        None => return, // already within the cap
    };
    name.truncate(cap_byte);
}

impl Styles {
    /// Return the active default style — the persisted value if
    /// present, otherwise the built-in defaults. Callers should
    /// use this rather than `.default.unwrap_or_default()` so the
    /// fallback rule is single-sourced.
    pub fn effective_default(&self) -> StyleEntry {
        self.default.clone().unwrap_or_default()
    }

    /// Return the active transparency settings — same single-
    /// source contract as [`Self::effective_default`].
    pub fn effective_transparency(&self) -> Transparency {
        self.transparency.clone().unwrap_or_default()
    }

    /// Read `styles.xml` from disk. Missing file → empty `Styles`
    /// (the `effective_*` getters then yield built-in defaults).
    /// Parse failure → `Err`; the caller decides whether to log
    /// and proceed with defaults or surface the error.
    ///
    /// Bounded read: a file exceeding [`STYLES_XML_MAX_BYTES`]
    /// is rejected before allocation so a maliciously-planted
    /// multi-gigabyte file at the styles path can't OOM the
    /// process at startup. After parsing, `font_name` is
    /// truncated to [`FONT_NAME_MAX_LEN`] characters as a
    /// second-layer defence against pathological input that
    /// slipped under the file-size cap.
    pub fn load_from_xml(path: &Path) -> Result<Self, StylesError> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(StylesError::Io(e.to_string())),
        };
        if meta.len() > STYLES_XML_MAX_BYTES {
            return Err(StylesError::Io(format!(
                "styles.xml is {} bytes (cap {}); refusing to read",
                meta.len(),
                STYLES_XML_MAX_BYTES,
            )));
        }
        let bytes = std::fs::read(path).map_err(|e| StylesError::Io(e.to_string()))?;
        let text = std::str::from_utf8(&bytes).map_err(|e| StylesError::Utf8(e.to_string()))?;
        let mut parsed: Styles =
            quick_xml::de::from_str(text).map_err(|e| StylesError::Parse(e.to_string()))?;
        if let Some(ref mut entry) = parsed.default {
            truncate_font_name(&mut entry.font_name);
        }
        Ok(parsed)
    }

    /// Write `styles.xml` atomically.
    ///
    /// Mirrors [`session::Session::save_to_xml`]: serialise to an
    /// in-memory string, write the bytes to a `NamedTempFile`
    /// anchored in the same directory, `sync_all` the temp, then
    /// `persist` (atomic same-filesystem rename) onto the target
    /// path. Guarantees:
    ///
    ///   - **Power-loss safety:** the rename is atomic at the
    ///     OS level — a crash mid-save leaves either the old
    ///     `styles.xml` intact or the new one fully written,
    ///     never a truncated file the next launch silently
    ///     reads as "no settings" via the `from_utf8` /
    ///     `quick_xml` error paths.
    ///   - **No stale tmp files:** if any step fails the
    ///     `NamedTempFile` drops itself and the temp file
    ///     vanishes; no `.styles.xml.tmp` siblings accumulate
    ///     on a flapping disk.
    ///
    /// Same direct write `File::create` + `write_all` pattern
    /// that lived here before was prone to leaving zero-byte
    /// or torn files if interrupted between truncate and the
    /// completion of the body write.
    pub fn save_to_xml(&self, path: &Path) -> Result<(), StylesError> {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let body =
            quick_xml::se::to_string(self).map_err(|e| StylesError::Serialize(e.to_string()))?;
        xml.push_str(&body);

        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent).map_err(|e| StylesError::Io(e.to_string()))?;
        }
        let parent_dir = parent.unwrap_or_else(|| Path::new("."));

        let mut tmp = tempfile::Builder::new()
            .prefix(".styles-")
            .suffix(".xml.tmp")
            .tempfile_in(parent_dir)
            .map_err(|e| StylesError::Io(e.to_string()))?;
        tmp.write_all(xml.as_bytes())
            .map_err(|e| StylesError::Io(e.to_string()))?;
        tmp.as_file_mut()
            .sync_all()
            .map_err(|e| StylesError::Io(e.to_string()))?;
        tmp.persist(path)
            .map_err(|e| StylesError::Io(e.error.to_string()))?;
        Ok(())
    }
}

/// Parse `RRGGBB` into the three byte components in display
/// order. Returns `None` if the input isn't exactly six hex
/// digits (length, character class, or both). Tolerates any
/// case. Strips an optional leading `#` so values pasted from a
/// browser colour picker work without manual editing.
pub fn parse_rgb_hex(s: &str) -> Option<(u8, u8, u8)> {
    let trimmed = s.strip_prefix('#').unwrap_or(s);
    if trimmed.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&trimmed[0..2], 16).ok()?;
    let g = u8::from_str_radix(&trimmed[2..4], 16).ok()?;
    let b = u8::from_str_radix(&trimmed[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Format `(r, g, b)` as `RRGGBB` (upper-case, no leading `#`).
/// The inverse of [`parse_rgb_hex`] for the canonical persisted
/// form.
pub fn format_rgb_hex(r: u8, g: u8, b: u8) -> String {
    format!("{r:02X}{g:02X}{b:02X}")
}

#[derive(Debug, Clone)]
pub enum StylesError {
    Io(String),
    Utf8(String),
    Parse(String),
    Serialize(String),
}

impl std::fmt::Display for StylesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StylesError::Io(e) => write!(f, "styles.xml I/O: {e}"),
            StylesError::Utf8(e) => write!(f, "styles.xml utf-8: {e}"),
            StylesError::Parse(e) => write!(f, "styles.xml parse: {e}"),
            StylesError::Serialize(e) => write!(f, "styles.xml serialize: {e}"),
        }
    }
}

impl std::error::Error for StylesError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("styles.xml");
        (dir, path)
    }

    #[test]
    fn default_styles_round_trip_empty_doc() {
        let (_dir, path) = temp_path();
        let s = Styles::default();
        s.save_to_xml(&path).unwrap();
        let loaded = Styles::load_from_xml(&path).unwrap();
        assert_eq!(s, loaded);
    }

    #[test]
    fn effective_default_falls_back_when_none() {
        let s = Styles::default();
        let d = s.effective_default();
        assert_eq!(d.font_name, "Courier New");
        assert_eq!(d.font_size, 10);
        assert_eq!(d.fg, "000000");
        assert_eq!(d.bg, "FFFFFF");
        assert!(!d.bold && !d.italic && !d.underline);
    }

    #[test]
    fn round_trip_full_default_style() {
        let (_dir, path) = temp_path();
        let s = Styles {
            default: Some(StyleEntry {
                font_name: "Consolas".into(),
                font_size: 12,
                bold: true,
                italic: false,
                underline: true,
                fg: "1E1E1E".into(),
                bg: "F5F5F5".into(),
            }),
            transparency: None,
        };
        s.save_to_xml(&path).unwrap();
        let loaded = Styles::load_from_xml(&path).unwrap();
        assert_eq!(s, loaded);
    }

    #[test]
    fn round_trip_transparency() {
        let (_dir, path) = temp_path();
        let s = Styles {
            default: None,
            transparency: Some(Transparency {
                enabled: true,
                percent: 75,
            }),
        };
        s.save_to_xml(&path).unwrap();
        let loaded = Styles::load_from_xml(&path).unwrap();
        assert_eq!(s, loaded);
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let (_dir, path) = temp_path();
        // Note: file deliberately not created.
        let loaded = Styles::load_from_xml(&path).unwrap();
        assert_eq!(loaded, Styles::default());
    }

    #[test]
    fn rgb_hex_round_trip() {
        assert_eq!(parse_rgb_hex("000000"), Some((0, 0, 0)));
        assert_eq!(parse_rgb_hex("FFFFFF"), Some((255, 255, 255)));
        assert_eq!(parse_rgb_hex("1E90FF"), Some((0x1E, 0x90, 0xFF)));
        assert_eq!(parse_rgb_hex("#1E90FF"), Some((0x1E, 0x90, 0xFF)));
        assert_eq!(parse_rgb_hex("1e90ff"), Some((0x1E, 0x90, 0xFF)));
        assert_eq!(format_rgb_hex(0x1E, 0x90, 0xFF), "1E90FF");
        // Round-trip via both directions.
        let (r, g, b) = parse_rgb_hex("ABCDEF").unwrap();
        assert_eq!(format_rgb_hex(r, g, b), "ABCDEF");
    }

    #[test]
    fn rgb_hex_rejects_malformed() {
        assert!(parse_rgb_hex("").is_none());
        assert!(parse_rgb_hex("12345").is_none()); // too short
        assert!(parse_rgb_hex("1234567").is_none()); // too long
        assert!(parse_rgb_hex("XYZXYZ").is_none()); // non-hex
        assert!(parse_rgb_hex("##FFFFFF").is_none()); // double prefix
    }

    #[test]
    fn oversized_styles_xml_is_rejected() {
        let (_dir, path) = temp_path();
        // Write a 5 KiB blob of valid-but-pathological XML that
        // would still parse cleanly if we let it through. The
        // file-size cap should reject it before parsing kicks in,
        // proving the guard fires regardless of the content's
        // shape.
        let filler: String = " ".repeat(STYLES_XML_MAX_BYTES as usize + 1);
        let xml =
            format!(r#"<?xml version="1.0"?><styles><default font_name="X"/></styles>{filler}"#);
        std::fs::write(&path, xml).unwrap();
        let err = Styles::load_from_xml(&path).unwrap_err();
        match err {
            StylesError::Io(msg) => assert!(msg.contains("cap"), "unexpected message: {msg}"),
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn oversized_font_name_is_truncated() {
        let (_dir, path) = temp_path();
        // A 500-char font name fits under the 4 KiB file cap but
        // exceeds `FONT_NAME_MAX_LEN`. The loader should truncate
        // on the way out so downstream Win32/Scintilla calls see
        // a bounded string.
        let huge_name = "A".repeat(500);
        let xml =
            format!(r#"<?xml version="1.0"?><styles><default font_name="{huge_name}"/></styles>"#);
        std::fs::write(&path, xml).unwrap();
        let loaded = Styles::load_from_xml(&path).unwrap();
        let entry = loaded.default.unwrap();
        assert_eq!(entry.font_name.chars().count(), FONT_NAME_MAX_LEN);
        assert!(entry.font_name.chars().all(|c| c == 'A'));
    }

    /// Multi-byte font name truncation must never split a UTF-8
    /// codepoint. A naive `String::truncate(FONT_NAME_MAX_LEN)`
    /// at the byte offset would panic for a font name in CJK or
    /// other multi-byte-per-char scripts; the char-aware helper
    /// finds the codepoint boundary correctly so the result is
    /// always valid UTF-8 with length ≤ FONT_NAME_MAX_LEN chars.
    #[test]
    fn oversized_font_name_truncation_is_codepoint_safe() {
        let (_dir, path) = temp_path();
        // 'ä' is 2 bytes in UTF-8; 500 of them = 1000 bytes — still
        // under the 4 KiB file cap, but over `FONT_NAME_MAX_LEN`
        // measured in chars (256).
        let huge_name: String = std::iter::repeat_n('ä', 500).collect();
        let xml =
            format!(r#"<?xml version="1.0"?><styles><default font_name="{huge_name}"/></styles>"#);
        std::fs::write(&path, xml).unwrap();
        let loaded = Styles::load_from_xml(&path).unwrap();
        let entry = loaded.default.unwrap();
        let chars: Vec<char> = entry.font_name.chars().collect();
        assert_eq!(chars.len(), FONT_NAME_MAX_LEN);
        assert!(chars.iter().all(|c| *c == 'ä'));
    }

    #[test]
    fn missing_attributes_use_defaults() {
        // Hand-crafted XML with `<default>` carrying only a subset
        // of attributes — verify serde fills the rest from
        // `default_*` functions rather than rejecting.
        let xml = r#"<?xml version="1.0"?>
<styles><default font_name="Fira Code"/></styles>"#;
        let parsed: Styles = quick_xml::de::from_str(xml).unwrap();
        let entry = parsed.default.unwrap();
        assert_eq!(entry.font_name, "Fira Code");
        assert_eq!(entry.font_size, 10); // default
        assert_eq!(entry.fg, "000000"); // default
        assert_eq!(entry.bg, "FFFFFF"); // default
        assert!(!entry.bold && !entry.italic && !entry.underline);
    }
}
