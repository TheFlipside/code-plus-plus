//! UDL XML serialiser — writes a [`UdlDefinition`] back to the same
//! `<NotepadPlus><UserLang>...</UserLang></NotepadPlus>` XML shape
//! that [`UdlDefinition::parse`] reads.
//!
//! # Round-trip contract
//!
//! A definition parsed from any UDL file (v2.0 or v2.1) and then
//! serialised via [`UdlDefinition::to_xml_string`] must re-parse to
//! a byte-equal in-memory value. This is asserted by the
//! `round_trip_*` tests below against the preinstalled Markdown
//! fixture, a Cisco-IOS-shaped v2.0 UDL, and an all-empty synthetic
//! UDL that stresses the empty-value branches.
//!
//! Fidelity is at the **normalised in-memory shape**, not byte-for-
//! byte against the source file. Two intentional normalisations
//! happen during round-trip:
//!
//! - **v2.0 → v2.1 keyword-list name aliases.** A v2.0 file's
//!   `<Keywords name="Numbers, prefixes">` becomes
//!   `<Keywords name="Numbers, prefix1">` on write. See
//!   [`crate::UdlKeywordLists::from_raw`] for the alias table.
//! - **Extension lowercasing / whitespace collapse.** Extensions
//!   are stored lowercase with single-space separators; a source
//!   `ext=" MD   Markdown "` round-trips as `ext="md markdown"`.
//!
//! Everything else — style-slot order, keyword-value contents,
//! hex-colour bytes, prefix-mode flags, dark-mode-theme attribute,
//! v0/1/2 numeric enum values — is preserved verbatim.
//!
//! # XML escaping
//!
//! Attribute values are escaped for `& < > " '` and the three
//! whitespace control characters `\t \r \n` (which XML attribute-
//! value normalisation would otherwise collapse to spaces on read —
//! see the XML 1.0 spec §3.3.3). Text content between
//! `<Keywords>...</Keywords>` tags is escaped for `& < >` and the
//! same three control characters (so a value like a raw newline
//! inside a keyword list survives the round-trip).
//!
//! **Illegal characters are stripped.** XML 1.0 §2.2 `Char`
//! production forbids C0 controls other than `\t \r \n`
//! (`0x00`–`0x08`, `0x0B`, `0x0C`, `0x0E`–`0x1F`) and the two
//! Unicode noncharacters `U+FFFE`/`U+FFFF` — these codepoints
//! cannot legally appear anywhere in an XML 1.0 document, and
//! unlike `\t \r \n` they can't even be represented as numeric
//! character references. If any of them reach the serialiser
//! (through a hostile UDL name / keyword value hand-authored to
//! attack a strict downstream consumer, OR embedded inside a
//! preserved preamble comment), they're silently dropped and a
//! `tracing::debug` line records the count. This mirrors the
//! sanitisation discipline `ui_win32::sanitize_udl_name_for_menu`
//! applies for the same threat model. The preamble is filtered on
//! **emission**, not capture — the field content on the parsed
//! [`UdlDefinition`] holds the raw bytes as read (useful for
//! diagnostics), and only the write to disk normalises them.
//!
//! Every other Unicode codepoint passes through unchanged — the
//! input is already valid Rust `&str`, so every byte is a valid
//! UTF-8 sequence and quick-xml handles the encoding declaration
//! on parse.

use std::fmt::Write as _;
use std::path::Path;

use crate::{UdlDefinition, UdlError, UdlKeywordLists, UdlStyle};

/// Indentation unit — four ASCII spaces per level. Matches the
/// convention every real-world UDL fixture uses (both N++'s own
/// writer and the community collection under
/// `notepad-plus-plus/userDefinedLanguages`), so a diff between a
/// hand-authored UDL and a Code++-round-tripped one has minimal
/// whitespace noise.
const INDENT: &str = "    ";

impl UdlDefinition {
    /// Serialise this definition to a UDL XML document string.
    ///
    /// Output is a self-contained `<NotepadPlus><UserLang>...`
    /// document with LF line endings, no XML declaration, no BOM,
    /// no DOCTYPE — matches what N++'s own UDL editor writes.
    ///
    /// [`UdlDefinition::parse`] on the returned string reproduces
    /// this exact value (subject to the two documented normalisations
    /// at [module level](self)).
    #[must_use]
    pub fn to_xml_string(&self) -> String {
        // 4 KiB baseline: the 24 style slots + 28 keyword slots each
        // contribute at least one line of markup, and every real
        // UDL fixture we've seen fits comfortably under this. A
        // maxed-out UDL with long keyword lists grows the buffer
        // organically via `push_str`; the initial capacity just
        // avoids reallocations for the common case.
        let mut out = String::with_capacity(4096);
        // Re-emit any leading comment prolog (`crate::UdlDefinition::preamble`)
        // captured on parse. Preserves the Edditoria MIT-licence
        // notice on the preinstalled Markdown UDL — see the field
        // docstring on `crate::UdlDefinition::preamble` for the
        // licensing-compliance rationale.
        //
        // Filtered through `push_preamble` so a hostile UDL whose
        // leading comment carries C0 controls or U+FFFE/U+FFFF
        // cannot use the preamble path to bypass the same
        // XML-1.0-illegal-character stripping `push_attr`/`push_text`
        // apply to the parsed model — the preamble field is `pub`
        // and populated from user-writable `userDefineLangs/`
        // content, same threat model.
        if let Some(preamble) = &self.preamble {
            push_preamble(&mut out, preamble);
        }
        out.push_str("<NotepadPlus>\n");
        write_user_lang(&mut out, self);
        out.push_str("</NotepadPlus>\n");
        out
    }

    /// Write this definition to `path` as UDL XML. Overwrites any
    /// existing file at that path.
    ///
    /// # Errors
    ///
    /// Returns [`UdlError::Io`] on filesystem write failure. The
    /// path is preserved on the error so a caller (the m3 editor
    /// modal) can surface a diagnostic naming the failed file
    /// without threading it separately.
    ///
    /// **Caller responsibility.** No path canonicalisation or
    /// containment check happens here — the m3 modal is expected
    /// to constrain `path` to `<config_dir>/userDefineLangs/`
    /// before calling, matching the discipline on
    /// [`UdlDefinition::from_file`] and mirroring the read-side
    /// containment check already implemented in
    /// [`crate::UdlRegistry::scan_dir`].
    ///
    /// **Non-atomic write.** Uses `std::fs::write`, which truncates
    /// then writes in place. A crash or disk-full event mid-write
    /// leaves a partially-written file. The m3 editor's save
    /// action is expected to layer atomic-write + backup on top
    /// of this primitive (temp file + rename, same pattern
    /// `crate::shell::fif` uses — DESIGN.md §7.4).
    pub fn save_to_file(&self, path: &Path) -> Result<(), UdlError> {
        let xml = self.to_xml_string();
        std::fs::write(path, xml.as_bytes()).map_err(|source| UdlError::Io {
            path: path.to_owned(),
            source,
        })
    }
}

fn write_user_lang(out: &mut String, udl: &UdlDefinition) {
    // Extensions are joined with single spaces to reconstruct the
    // `ext="..."` attribute. The parser splits on whitespace so each
    // element in the Vec should already be whitespace-free. If a
    // caller (e.g. an m3-editor UI bug) ever pushes an entry
    // containing whitespace, the join would silently glue two tokens
    // into one and reparse would silently split them back apart —
    // a data-shape corruption with no error. Trip early in debug so
    // the bug surfaces during development.
    debug_assert!(
        udl.extensions
            .iter()
            .all(|e| !e.chars().any(char::is_whitespace)),
        "UDL extensions must not contain internal whitespace; got {:?}",
        udl.extensions,
    );

    out.push_str(INDENT);
    out.push_str("<UserLang name=\"");
    push_attr(out, &udl.name);
    out.push_str("\" ext=\"");
    push_attr(out, &udl.extensions.join(" "));
    out.push_str("\" udlVersion=\"");
    push_attr(out, &udl.udl_version);
    out.push('"');
    if udl.dark_mode_theme {
        out.push_str(" darkModeTheme=\"yes\"");
    }
    out.push_str(">\n");

    write_settings(out, udl);
    write_keyword_lists(out, &udl.keyword_lists);
    write_styles(out, &udl.styles);

    out.push_str(INDENT);
    out.push_str("</UserLang>\n");
}

fn write_settings(out: &mut String, udl: &UdlDefinition) {
    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str("<Settings>\n");

    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str(INDENT);
    let _ = writeln!(
        out,
        "<Global caseIgnored=\"{}\" allowFoldOfComments=\"{}\" foldCompact=\"{}\" \
         forcePureLC=\"{}\" decimalSeparator=\"{}\" />",
        yes_no(udl.settings.case_ignored),
        yes_no(udl.settings.allow_fold_of_comments),
        yes_no(udl.settings.fold_compact),
        udl.settings.force_pure_lc,
        udl.settings.decimal_separator,
    );

    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str("<Prefix");
    for (i, flag) in udl.prefix.iter().enumerate() {
        // 1-based (`Keywords1`..=`Keywords8`) matches N++.
        let _ = write!(out, " Keywords{}=\"{}\"", i + 1, yes_no(*flag));
    }
    out.push_str(" />\n");

    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str("</Settings>\n");
}

fn write_keyword_lists(out: &mut String, kw: &UdlKeywordLists) {
    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str("<KeywordLists>\n");

    // Emit in the canonical v2.1 order matching every N++ fixture
    // and the community collection. The order is load-bearing for
    // human diff-ability: reordering would produce noisy diffs
    // against hand-authored UDLs even when semantics are equal.
    write_keyword(out, "Comments", &kw.comments);
    write_keyword(out, "Numbers, prefix1", &kw.numbers_prefix1);
    write_keyword(out, "Numbers, prefix2", &kw.numbers_prefix2);
    write_keyword(out, "Numbers, extras1", &kw.numbers_extras1);
    write_keyword(out, "Numbers, extras2", &kw.numbers_extras2);
    write_keyword(out, "Numbers, suffix1", &kw.numbers_suffix1);
    write_keyword(out, "Numbers, suffix2", &kw.numbers_suffix2);
    write_keyword(out, "Numbers, range", &kw.numbers_range);
    write_keyword(out, "Operators1", &kw.operators1);
    write_keyword(out, "Operators2", &kw.operators2);
    write_keyword(out, "Folders in code1, open", &kw.folders_in_code1_open);
    write_keyword(out, "Folders in code1, middle", &kw.folders_in_code1_middle);
    write_keyword(out, "Folders in code1, close", &kw.folders_in_code1_close);
    write_keyword(out, "Folders in code2, open", &kw.folders_in_code2_open);
    write_keyword(out, "Folders in code2, middle", &kw.folders_in_code2_middle);
    write_keyword(out, "Folders in code2, close", &kw.folders_in_code2_close);
    write_keyword(out, "Folders in comment, open", &kw.folders_in_comment_open);
    write_keyword(
        out,
        "Folders in comment, middle",
        &kw.folders_in_comment_middle,
    );
    write_keyword(
        out,
        "Folders in comment, close",
        &kw.folders_in_comment_close,
    );
    for (i, value) in kw.keywords.iter().enumerate() {
        // 1-based (`Keywords1`..=`Keywords8`) matches N++.
        let mut name = String::with_capacity(9);
        let _ = write!(&mut name, "Keywords{}", i + 1);
        write_keyword(out, &name, value);
    }
    write_keyword(out, "Delimiters", &kw.delimiters);

    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str("</KeywordLists>\n");
}

fn write_keyword(out: &mut String, name: &str, value: &str) {
    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str("<Keywords name=\"");
    push_attr(out, name);
    out.push_str("\">");
    // Empty values render as `<Keywords name="X"></Keywords>` (not
    // self-closing), matching what the N++ editor writes for
    // every unused slot in every fixture we've inspected. A
    // self-closing `<Keywords name="X" />` also parses fine on
    // read, but produces spurious diffs against hand-authored
    // files that always use the open/close form.
    push_text(out, value);
    out.push_str("</Keywords>\n");
}

fn write_styles(out: &mut String, styles: &[UdlStyle]) {
    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str("<Styles>\n");
    for style in styles {
        out.push_str(INDENT);
        out.push_str(INDENT);
        out.push_str(INDENT);
        out.push_str("<WordsStyle name=\"");
        push_attr(out, &style.name);
        out.push_str("\" fgColor=\"");
        push_hex_color(out, style.fg_color);
        out.push_str("\" bgColor=\"");
        push_hex_color(out, style.bg_color);
        out.push_str("\" fontName=\"");
        push_attr(out, &style.font_name);
        let _ = writeln!(
            out,
            "\" fontStyle=\"{}\" nesting=\"{}\" />",
            style.font_style, style.nesting
        );
    }
    out.push_str(INDENT);
    out.push_str(INDENT);
    out.push_str("</Styles>\n");
}

/// XML attribute-value escaping. Covers `& < > " '` (structural
/// delimiters) plus `\t \r \n` (whitespace control characters that
/// XML 1.0 §3.3.3 attribute-value normalisation replaces with a
/// space during parsing — so a keyword literal containing a real
/// TAB must be encoded as `&#x9;` to survive the round-trip).
///
/// XML 1.0 `Char`-illegal codepoints (see [`is_xml_char`]) are
/// **silently dropped** — same threat model, and defence, as
/// `ui_win32::sanitize_udl_name_for_menu`. If any are dropped a
/// `tracing::debug` line records the count on the containing UDL.
fn push_attr(out: &mut String, value: &str) {
    let mut dropped = 0usize;
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' => out.push_str("&#x9;"),
            '\r' => out.push_str("&#xD;"),
            '\n' => out.push_str("&#xA;"),
            other if is_xml_char(other) => out.push(other),
            _ => dropped += 1,
        }
    }
    if dropped > 0 {
        tracing::debug!(
            dropped,
            "UDL serialise: dropped XML-1.0-illegal characters from attribute value"
        );
    }
}

/// XML text-content escaping. Covers `& < >` plus the same three
/// whitespace control characters, so a `<Keywords>...</Keywords>`
/// body containing a literal newline round-trips (the value is
/// preserved verbatim by quick-xml on read; escaping it here just
/// keeps the output single-line and diff-friendly).
///
/// `"` and `'` are NOT escaped in text content — they're legal
/// characters between element tags. Escaping them anyway would
/// pass a re-parse but produce visual noise against hand-authored
/// files.
///
/// XML 1.0 `Char`-illegal codepoints (see [`is_xml_char`]) are
/// **silently dropped** — same discipline as [`push_attr`].
fn push_text(out: &mut String, value: &str) {
    let mut dropped = 0usize;
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' => out.push_str("&#x9;"),
            '\r' => out.push_str("&#xD;"),
            '\n' => out.push_str("&#xA;"),
            other if is_xml_char(other) => out.push(other),
            _ => dropped += 1,
        }
    }
    if dropped > 0 {
        tracing::debug!(
            dropped,
            "UDL serialise: dropped XML-1.0-illegal characters from text content"
        );
    }
}

/// Emit a preamble string with XML-1.0-illegal codepoints stripped.
///
/// Unlike [`push_attr`] and [`push_text`], the preamble is XML
/// markup we captured verbatim (comment delimiters, angle
/// brackets, ampersands) — escaping those characters would break
/// the round-trip. This function performs the illegal-character
/// filter and nothing else: any codepoint that fails
/// [`is_xml_char`] is dropped; every other byte passes through.
///
/// The [`UdlDefinition::preamble`] field is `pub` and populated
/// from user-writable UDL content, so a hostile leading comment
/// containing a raw NUL byte or `U+FFFE` must not survive to
/// disk. The [`push_attr`]/[`push_text`] discipline covers the
/// parsed model; this covers the preamble path.
fn push_preamble(out: &mut String, preamble: &str) {
    let mut dropped = 0usize;
    for ch in preamble.chars() {
        if is_xml_char(ch) {
            out.push(ch);
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        tracing::debug!(
            dropped,
            "UDL serialise: dropped XML-1.0-illegal characters from preamble"
        );
    }
}

/// XML 1.0 §2.2 `Char` production: which Unicode codepoints are
/// legal anywhere in an XML 1.0 document.
///
/// `Char ::= #x9 | #xA | #xD | [#x20-#xD7FF] | [#xE000-#xFFFD]
///          | [#x10000-#x10FFFF]`
///
/// Rust `char` cannot represent surrogates (`0xD800`–`0xDFFF`) so
/// the surrogate range doesn't need explicit checking. The
/// remaining exclusions are the C0 control range minus `\t \r \n`
/// (`0x00`–`0x08`, `0x0B`, `0x0C`, `0x0E`–`0x1F`) and the two
/// noncharacters `U+FFFE`/`U+FFFF`.
///
/// **Illegal characters cannot be recovered via numeric character
/// references** in XML 1.0 — `&#x0;` is itself invalid. So the
/// only safe strategy is to strip them from the output entirely.
/// XML 1.1 relaxes this, but quick-xml and every consumer we care
/// about (including real Notepad++) operates in XML 1.0 mode.
fn is_xml_char(c: char) -> bool {
    matches!(
        u32::from(c),
        0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x0010_FFFF
    )
}

/// Serialise a 24-bit RGB colour as 6-digit uppercase hex, matching
/// what N++ writes and what every fixture uses. Leading zeros are
/// preserved: `0x00_FF_00` renders as `"00FF00"`, not `"FF00"`.
///
/// The high byte of the input is masked to zero — the in-memory
/// [`UdlStyle::fg_color`] is documented as 24-bit RGB. If an alpha
/// or otherwise-set high byte reaches us anyway, dropping it here
/// prevents a 7-or-8-digit output that N++ would reject on read.
fn push_hex_color(out: &mut String, color: u32) {
    let _ = write!(out, "{:06X}", color & 0x00FF_FFFF);
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to the preinstalled markdown UDL fixture. Mirrors the
    /// resolver in `lib.rs` so tests work from any working
    /// directory.
    fn markdown_fixture_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("preinstalled-udls")
            .join("markdown._preinstalled.udl.xml")
    }

    /// Core round-trip assertion: `parse → serialise → parse` must
    /// produce a byte-equal in-memory value. `source_path` is set
    /// on the initial read but not on the string re-parse, so
    /// compare after clearing it on both sides.
    fn assert_round_trip(mut udl: UdlDefinition) {
        udl.source_path = None;
        let xml = udl.to_xml_string();
        let mut reparsed = UdlDefinition::parse(&xml)
            .unwrap_or_else(|err| panic!("serialised output failed to re-parse: {err}\n\n{xml}"));
        reparsed.source_path = None;
        assert_eq!(
            udl, reparsed,
            "round-trip changed the in-memory shape\n\nserialised:\n{xml}"
        );
    }

    #[test]
    fn round_trip_markdown_preinstalled_fixture() {
        let udl =
            UdlDefinition::from_file(&markdown_fixture_path()).expect("fixture must parse cleanly");
        assert_round_trip(udl);
    }

    #[test]
    fn round_trip_cisco_ios_shaped_v2_0_udl() {
        // Same reduction used by `lib.rs::cisco_ios_shaped_v2_0_udl_loads_end_to_end`.
        // Covers v2.0 aliases (numbers_extras2 via "Numbers, additional"),
        // empty `ext`, and populated `<Styles>`.
        let xml = r#"<NotepadPlus>
            <UserLang name="Cisco IOS" ext="" udlVersion="2.0">
              <Settings>
                <Global caseIgnored="yes" allowFoldOfComments="no"
                        forcePureLC="0" foldCompact="no" />
                <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                        Keywords4="no" Keywords5="yes" Keywords6="no"
                        Keywords7="no" Keywords8="no" />
              </Settings>
              <KeywordLists>
                <Keywords name="Comments">00! 00remark 01 02((EOF)) 03 04</Keywords>
                <Keywords name="Numbers, additional">:</Keywords>
                <Keywords name="Operators1">. /</Keywords>
                <Keywords name="Keywords1">interface hostname</Keywords>
                <Keywords name="Keywords5">FastEthernet Vlan</Keywords>
                <Keywords name="Delimiters">00 01 02 03 04 05 06 07 08 09 10 11 12 13 14 15 16 17 18 19 20 21 22 23</Keywords>
              </KeywordLists>
              <Styles>
                <WordsStyle name="DEFAULT" fgColor="000000" bgColor="FFFFFF" fontName="" fontStyle="0" nesting="0" />
                <WordsStyle name="KEYWORDS1" fgColor="AC8202" bgColor="FFFFFF" fontName="" fontStyle="1" nesting="0" />
                <WordsStyle name="KEYWORDS5" fgColor="0080FF" bgColor="FFFFFF" fontName="" fontStyle="0" nesting="0" />
              </Styles>
            </UserLang>
          </NotepadPlus>"#;
        let udl = UdlDefinition::parse(xml).expect("Cisco-IOS-shaped v2.0 UDL must parse");
        assert_round_trip(udl);
    }

    #[test]
    fn round_trip_all_empty_udl() {
        // Stress the empty-value branches: every keyword slot empty,
        // no styles, minimum settings. This is what the m3 editor's
        // "New UDL" starting point will look like.
        let xml = r#"<NotepadPlus>
            <UserLang name="Empty" ext="" udlVersion="2.1">
              <Settings>
                <Global caseIgnored="no" allowFoldOfComments="no"
                        foldCompact="no" forcePureLC="0" decimalSeparator="0" />
                <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                        Keywords4="no" Keywords5="no" Keywords6="no"
                        Keywords7="no" Keywords8="no" />
              </Settings>
              <KeywordLists />
              <Styles />
            </UserLang>
          </NotepadPlus>"#;
        let udl = UdlDefinition::parse(xml).expect("all-empty UDL must parse");
        assert_round_trip(udl);
    }

    #[test]
    fn round_trip_dark_mode_theme_attribute() {
        // The `darkModeTheme="yes"` attribute appears on Edditoria's
        // dark-mode markdown variants. Ensure round-trip preserves it.
        let xml = r#"<NotepadPlus>
            <UserLang name="Dark" ext="dark" udlVersion="2.1" darkModeTheme="yes">
              <Settings>
                <Global caseIgnored="no" allowFoldOfComments="no"
                        foldCompact="no" forcePureLC="0" decimalSeparator="0" />
                <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                        Keywords4="no" Keywords5="no" Keywords6="no"
                        Keywords7="no" Keywords8="no" />
              </Settings>
              <KeywordLists />
              <Styles />
            </UserLang>
          </NotepadPlus>"#;
        let udl = UdlDefinition::parse(xml).expect("dark-mode UDL must parse");
        assert!(udl.dark_mode_theme, "fixture attr should populate");
        let serialised = udl.to_xml_string();
        assert!(
            serialised.contains("darkModeTheme=\"yes\""),
            "dark-mode attribute must appear in serialised output;\n{serialised}"
        );
        assert_round_trip(udl);
    }

    #[test]
    fn dark_mode_theme_attribute_omitted_when_false() {
        // Symmetry with the above: the light-mode UDL must NOT emit
        // `darkModeTheme="no"` — N++'s writer omits the attribute
        // entirely when it doesn't apply, and every light fixture
        // matches that convention.
        let mut udl = UdlDefinition::parse(
            r#"<NotepadPlus>
                <UserLang name="Light" ext="l" udlVersion="2.1">
                  <Settings>
                    <Global caseIgnored="no" allowFoldOfComments="no"
                            foldCompact="no" forcePureLC="0" decimalSeparator="0" />
                    <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                            Keywords4="no" Keywords5="no" Keywords6="no"
                            Keywords7="no" Keywords8="no" />
                  </Settings>
                  <KeywordLists />
                  <Styles />
                </UserLang>
              </NotepadPlus>"#,
        )
        .expect("light UDL must parse");
        udl.dark_mode_theme = false;
        let serialised = udl.to_xml_string();
        assert!(
            !serialised.contains("darkModeTheme"),
            "darkModeTheme attribute must be omitted when false;\n{serialised}"
        );
    }

    #[test]
    fn xml_special_characters_are_escaped_in_attributes() {
        // The `name` attribute is user-controlled through the m3
        // editor. A hostile / typo'd name containing structural XML
        // characters must escape correctly so the output re-parses.
        let mut udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        udl.name = "A & B <c> \"d\" 'e'".to_owned();
        let xml = udl.to_xml_string();
        // Attribute-value delimiters: `"` MUST be encoded (we quote
        // attrs with double quotes). `&` MUST be encoded. `<` MUST
        // be encoded (parser would treat as tag-start).
        assert!(xml.contains("A &amp; B &lt;c&gt; &quot;d&quot; &apos;e&apos;"));
        // Then confirm the value round-trips through re-parse.
        let reparsed = UdlDefinition::parse(&xml).expect("escaped attrs must re-parse");
        assert_eq!(reparsed.name, "A & B <c> \"d\" 'e'");
    }

    #[test]
    fn xml_control_characters_are_escaped_in_attributes() {
        // XML §3.3.3 attribute-value normalisation replaces raw
        // `\t \r \n` in attributes with spaces on read. A UDL with
        // one of those bytes in its `name` would silently lose the
        // control character on round-trip. Escape as numeric
        // character references so the round-trip preserves them.
        //
        // Same discipline as the sanitizer test in the parser
        // (`sanitize_udl_name_for_menu`): a hostile UDL name that
        // embeds ANSI escape sequences or backspaces mustn't be
        // laundered by the round-trip into a subtly different
        // string.
        let mut udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        udl.name = "line1\nline2\ttab\rcr".to_owned();
        let xml = udl.to_xml_string();
        assert!(
            xml.contains("line1&#xA;line2&#x9;tab&#xD;cr"),
            "control characters must be escaped as numeric character references;\n{xml}"
        );
        let reparsed =
            UdlDefinition::parse(&xml).expect("control-char-escaped attrs must re-parse");
        assert_eq!(reparsed.name, "line1\nline2\ttab\rcr");
    }

    #[test]
    fn xml_special_characters_are_escaped_in_keyword_values() {
        // The keyword-list bodies are user-controlled. Common
        // examples: markdown's `Operators1` list contains literal
        // `<` and `>` bytes, so they MUST be escaped in text
        // content. The fixture is already exercised by
        // `round_trip_markdown_preinstalled_fixture`; this test
        // isolates the escaping rule so a regression surfaces at
        // the primitive level.
        let mut udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        udl.keyword_lists.keywords[0] = "< > & value".to_owned();
        let xml = udl.to_xml_string();
        assert!(
            xml.contains(">&lt; &gt; &amp; value</Keywords>"),
            "text content must escape `<`, `>`, `&`;\n{xml}"
        );
        let reparsed = UdlDefinition::parse(&xml).expect("escaped text must re-parse");
        assert_eq!(reparsed.keyword_lists.keywords[0], "< > & value");
    }

    #[test]
    fn hex_colours_are_six_digit_uppercase_with_leading_zeros() {
        // `push_hex_color` is width-6 uppercase; check leading-zero
        // handling and the high-byte mask.
        let mut buf = String::new();
        push_hex_color(&mut buf, 0x0000_0000);
        assert_eq!(buf, "000000");
        buf.clear();
        push_hex_color(&mut buf, 0x0000_FF00);
        assert_eq!(buf, "00FF00");
        buf.clear();
        push_hex_color(&mut buf, 0x00FF_FFFF);
        assert_eq!(buf, "FFFFFF");
        buf.clear();
        // High byte set — should be masked to 24-bit RGB.
        push_hex_color(&mut buf, 0xAB12_3456);
        assert_eq!(buf, "123456", "high byte must be masked out");
    }

    #[test]
    fn extensions_round_trip_lowercased_and_single_space_separated() {
        // The parser lowercases + splits on whitespace; the
        // serialiser joins with a single space. Together this means
        // `"MD   Markdown"` → parse → `["md", "markdown"]` →
        // serialise → `"md markdown"`. Pin the flow so a future
        // parser tweak doesn't silently break editor round-trips.
        let mut udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        udl.extensions = vec!["md".to_owned(), "markdown".to_owned()];
        let xml = udl.to_xml_string();
        assert!(xml.contains("ext=\"md markdown\""), "xml was:\n{xml}");
    }

    #[test]
    fn all_28_keyword_slots_are_emitted_even_when_empty() {
        // Every N++ fixture emits all 28 slots. Match that so a
        // hand-authored empty UDL and a Code++-round-tripped one
        // diff cleanly.
        let udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        let xml = udl.to_xml_string();
        // 19 named slots + 8 `Keywords1`..=`Keywords8` + Delimiters
        // = 28. Count occurrences of `<Keywords name=` for the pin.
        let count = xml.matches("<Keywords name=\"").count();
        assert_eq!(count, 28, "must emit all 28 slots;\n{xml}");
    }

    #[test]
    fn all_style_slots_round_trip_in_document_order() {
        // Order preservation is important for human-diff-ability;
        // reordering would show up as noise even when semantics are
        // equal. Build a UDL with a distinctive slot order and
        // check the output preserves it.
        let mut udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        udl.styles = vec![
            UdlStyle {
                name: "DELIMITERS3".to_owned(),
                fg_color: 0x11_22_33,
                bg_color: 0x44_55_66,
                font_name: String::new(),
                font_style: 0,
                nesting: 0,
            },
            UdlStyle {
                name: "DEFAULT".to_owned(),
                fg_color: 0xAA_BB_CC,
                bg_color: 0xDD_EE_FF,
                font_name: "Consolas".to_owned(),
                font_style: 7, // bold + italic + underline
                nesting: 0,
            },
        ];
        let xml = udl.to_xml_string();
        let delim_pos = xml.find("name=\"DELIMITERS3\"").expect("delim3 emitted");
        let default_pos = xml.find("name=\"DEFAULT\"").expect("default emitted");
        assert!(
            delim_pos < default_pos,
            "styles must retain input order (DELIMITERS3 first, DEFAULT second)"
        );
        // Also verify the fontName and fontStyle bit round-trip.
        assert!(xml.contains("fontName=\"Consolas\" fontStyle=\"7\""));
    }

    #[test]
    fn save_to_file_writes_bytes_that_round_trip() {
        let udl =
            UdlDefinition::from_file(&markdown_fixture_path()).expect("fixture must parse cleanly");

        let tmp = std::env::temp_dir().join(format!(
            "codepp-udl-save-to-file-{}.udl.xml",
            std::process::id()
        ));
        udl.save_to_file(&tmp).expect("save_to_file must succeed");
        // Read raw bytes so we can pin licence-header preservation
        // before the reparse strips it back down to the parsed
        // shape.
        let saved_bytes = std::fs::read_to_string(&tmp).expect("saved file must be readable");
        // Reparse from disk and assert equality.
        let reloaded = UdlDefinition::from_file(&tmp).expect("saved file must reload");
        // Cleanup regardless of the assertion outcome below.
        let _ = std::fs::remove_file(&tmp);

        // Licence-header preservation (Phase 4.6 m3a blocker fix):
        // Edditoria's MIT copyright notice is a leading XML comment
        // in the preinstalled fixture. Without preamble
        // preservation, save-in-place would strip it — a
        // redistribution-licence-compliance regression.
        assert!(
            saved_bytes.contains("Copyright (c) Edditoria"),
            "Edditoria's MIT copyright notice must survive the save round-trip; \
             this is a licence-compliance regression pin. Saved file starts with:\n{}",
            saved_bytes.chars().take(400).collect::<String>(),
        );

        // Both sides carry their own `source_path` (fixture vs
        // tmpfile); zero them for the comparison.
        let mut a = udl;
        a.source_path = None;
        let mut b = reloaded;
        b.source_path = None;
        assert_eq!(a, b);
    }

    #[test]
    fn c0_control_characters_are_stripped_from_attribute_values() {
        // XML 1.0 forbids C0 controls other than \t \r \n anywhere
        // in the document and does not permit them as numeric
        // character references. Attempting to write them into the
        // `name` attribute must drop them silently (same discipline
        // as `ui_win32::sanitize_udl_name_for_menu`), not emit an
        // invalid XML document. Verified by injecting the full
        // forbidden C0 subset plus U+FFFE/U+FFFF into `name` and
        // asserting each stripped byte is absent from the output.
        let mut udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        let mut hostile = String::from("safe");
        for byte in 0x00_u32..=0x08 {
            hostile.push(char::from_u32(byte).unwrap());
        }
        hostile.push('\u{0B}');
        hostile.push('\u{0C}');
        for byte in 0x0E_u32..=0x1F {
            hostile.push(char::from_u32(byte).unwrap());
        }
        hostile.push('\u{FFFE}');
        hostile.push('\u{FFFF}');
        hostile.push_str("tail");
        udl.name.clone_from(&hostile);

        let xml = udl.to_xml_string();
        // Every stripped codepoint must be absent as a raw byte.
        for ch in hostile.chars() {
            if !is_xml_char(ch) {
                assert!(
                    !xml.contains(ch),
                    "raw XML-illegal char U+{:04X} must not appear in output;\n{}",
                    u32::from(ch),
                    xml,
                );
            }
        }
        // Safe surrounding text must survive.
        assert!(xml.contains("safe"), "leading safe text must survive");
        assert!(xml.contains("tail"), "trailing safe text must survive");

        // The output must still re-parse (proves we didn't emit
        // invalid XML by escaping the forbidden chars via NCRs).
        let reparsed = UdlDefinition::parse(&xml).expect("stripped output must re-parse");
        assert_eq!(reparsed.name, "safetail", "only safe substring survives");
    }

    #[test]
    fn c0_control_characters_are_stripped_from_text_content() {
        // Same discipline as attribute values, applied to keyword-
        // list body content.
        let mut udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        udl.keyword_lists.keywords[0] = "a\u{01}b\u{FFFE}c".to_owned();
        let xml = udl.to_xml_string();
        assert!(
            !xml.contains('\u{01}'),
            "raw NUL-adjacent byte must be stripped"
        );
        assert!(!xml.contains('\u{FFFE}'), "noncharacter must be stripped");
        let reparsed = UdlDefinition::parse(&xml).expect("stripped output must re-parse");
        assert_eq!(reparsed.keyword_lists.keywords[0], "abc");
    }

    #[test]
    fn is_xml_char_matches_xml_1_0_char_production() {
        // Sanity-pin the predicate against a handful of boundary
        // codepoints. If a future refactor rewrites the ranges we
        // want the boundary drift to surface here, not silently in
        // production.
        assert!(!is_xml_char('\u{00}'), "NUL is illegal");
        assert!(!is_xml_char('\u{08}'), "0x08 is illegal");
        assert!(is_xml_char('\t'), "TAB is legal");
        assert!(is_xml_char('\n'), "LF is legal");
        assert!(!is_xml_char('\u{0B}'), "0x0B is illegal");
        assert!(!is_xml_char('\u{0C}'), "0x0C is illegal");
        assert!(is_xml_char('\r'), "CR is legal");
        assert!(!is_xml_char('\u{0E}'), "0x0E is illegal");
        assert!(!is_xml_char('\u{1F}'), "0x1F is illegal");
        assert!(is_xml_char(' '), "SPACE is legal");
        assert!(is_xml_char('a'), "ASCII letter is legal");
        assert!(is_xml_char('\u{FFFD}'), "REPLACEMENT CHARACTER is legal");
        assert!(!is_xml_char('\u{FFFE}'), "noncharacter U+FFFE is illegal");
        assert!(!is_xml_char('\u{FFFF}'), "noncharacter U+FFFF is illegal");
        assert!(is_xml_char('\u{10000}'), "first astral is legal");
        assert!(is_xml_char('\u{10FFFF}'), "last legal codepoint");
    }

    #[test]
    fn round_trip_preserves_leading_comment_prolog() {
        // Preamble preservation regression pin. Feed a UDL whose
        // XML starts with a distinctive comment and confirm the
        // comment survives parse → serialise → parse.
        let source = concat!(
            "<!--\n",
            "  Multi-line comment.\n",
            "  Copyright (c) Test Author. Some Licence.\n",
            "-->\n",
            r#"<NotepadPlus>
                <UserLang name="With Preamble" ext="wp" udlVersion="2.1">
                  <Settings>
                    <Global caseIgnored="no" allowFoldOfComments="no"
                            foldCompact="no" forcePureLC="0" decimalSeparator="0" />
                    <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                            Keywords4="no" Keywords5="no" Keywords6="no"
                            Keywords7="no" Keywords8="no" />
                  </Settings>
                  <KeywordLists />
                  <Styles />
                </UserLang>
              </NotepadPlus>"#,
        );
        let udl = UdlDefinition::parse(source).expect("preamble UDL must parse");
        assert!(
            udl.preamble.is_some(),
            "leading comment must populate preamble field"
        );
        assert!(
            udl.preamble
                .as_deref()
                .unwrap()
                .contains("Copyright (c) Test Author"),
            "preamble must capture the comment content"
        );

        let xml = udl.to_xml_string();
        assert!(
            xml.contains("Copyright (c) Test Author"),
            "serialised output must re-emit the comment;\n{xml}"
        );

        // Re-parse and compare (source_path is None on both sides
        // since we're going through `parse` not `from_file`).
        let reparsed = UdlDefinition::parse(&xml).expect("re-parse must succeed");
        assert_eq!(udl.preamble, reparsed.preamble);
    }

    #[test]
    fn round_trip_multiple_leading_comments() {
        // The extractor must capture all consecutive comment
        // blocks + their inter-block whitespace, not just the
        // first. Guards against a regression where the second
        // block is silently dropped.
        let source = concat!(
            "<!-- first comment -->\n",
            "<!-- second comment -->\n",
            r#"<NotepadPlus>
                <UserLang name="Multi Preamble" ext="mp" udlVersion="2.1">
                  <Settings>
                    <Global caseIgnored="no" allowFoldOfComments="no"
                            foldCompact="no" forcePureLC="0" decimalSeparator="0" />
                    <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                            Keywords4="no" Keywords5="no" Keywords6="no"
                            Keywords7="no" Keywords8="no" />
                  </Settings>
                  <KeywordLists />
                  <Styles />
                </UserLang>
              </NotepadPlus>"#,
        );
        let udl = UdlDefinition::parse(source).expect("multi-comment UDL must parse");
        let preamble = udl.preamble.as_deref().expect("preamble must populate");
        assert!(preamble.contains("first comment"));
        assert!(preamble.contains("second comment"));

        // Round-trip.
        let xml = udl.to_xml_string();
        let reparsed = UdlDefinition::parse(&xml).expect("re-parse must succeed");
        assert_eq!(udl.preamble, reparsed.preamble);
    }

    #[test]
    fn c0_control_characters_are_stripped_from_preamble() {
        // Regression pin for the m3a re-review blocker: the preamble
        // path must apply the same XML-1.0-Char sanitisation as
        // `push_attr`/`push_text`. A hostile UDL whose leading
        // comment carries a raw NUL / other C0 control / U+FFFE
        // could otherwise use the preamble path to write invalid
        // XML to disk, bypassing the sanitisation on the parsed
        // model.
        //
        // We construct the hostile preamble in-memory (rather than
        // parsing an XML string with raw C0 bytes — quick-xml
        // would reject some of them) because the field is `pub`
        // and any caller can populate it directly.
        let mut udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        udl.preamble = Some("<!-- pre\u{00}mble\u{01}\u{FFFE}text -->\n".to_owned());

        let xml = udl.to_xml_string();
        assert!(
            !xml.contains('\u{00}'),
            "raw NUL from preamble must be stripped;\n{xml}"
        );
        assert!(
            !xml.contains('\u{01}'),
            "raw 0x01 from preamble must be stripped;\n{xml}"
        );
        assert!(
            !xml.contains('\u{FFFE}'),
            "noncharacter from preamble must be stripped;\n{xml}"
        );
        // Safe bytes surrounding the stripped chars survive.
        assert!(xml.contains("<!-- pre"), "preamble start survives;\n{xml}");
        assert!(xml.contains("text -->"), "preamble end survives;\n{xml}");
        // Output must still parse (proof we didn't emit invalid XML).
        UdlDefinition::parse(&xml).expect("sanitised output must re-parse");
    }

    #[test]
    fn no_leading_comment_leaves_preamble_none() {
        // Pin the symmetric case: a UDL that starts directly with
        // `<NotepadPlus>` (or with only whitespace) has
        // `preamble = None`, and round-trip does not synthesise
        // a preamble.
        let udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        assert!(
            udl.preamble.is_none(),
            "no leading comment must leave preamble = None"
        );
        let xml = udl.to_xml_string();
        assert!(
            xml.starts_with("<NotepadPlus>\n"),
            "serialiser must not synthesise a preamble;\n{xml}"
        );
    }

    #[test]
    fn save_to_file_returns_io_error_with_path_on_write_failure() {
        // Point at a directory that cannot exist (a subdirectory of
        // a non-existent parent), so the underlying `fs::write`
        // fails. Confirm the error preserves the path for
        // diagnostics.
        let udl = UdlDefinition::parse(minimum_udl()).expect("minimum UDL must parse");
        let bad = std::path::Path::new("does/not/exist/udl.xml");
        let err = udl.save_to_file(bad).expect_err("write must fail");
        match err {
            UdlError::Io { path, .. } => assert_eq!(path, bad),
            other => panic!("expected UdlError::Io, got {other:?}"),
        }
    }

    /// A minimum valid UDL body used across the escape tests. Every
    /// slot empty, no styles, minimum settings — the "New UDL"
    /// starting point.
    fn minimum_udl() -> &'static str {
        r#"<NotepadPlus>
            <UserLang name="Min" ext="min" udlVersion="2.1">
              <Settings>
                <Global caseIgnored="no" allowFoldOfComments="no"
                        foldCompact="no" forcePureLC="0" decimalSeparator="0" />
                <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                        Keywords4="no" Keywords5="no" Keywords6="no"
                        Keywords7="no" Keywords8="no" />
              </Settings>
              <KeywordLists />
              <Styles />
            </UserLang>
          </NotepadPlus>"#
    }
}
