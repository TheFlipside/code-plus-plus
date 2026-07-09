//! UDL container-lexer painting helpers.
//!
//! Phase 4.6 m1c-3b-1: given a [`UdlDefinition`] and an active
//! [`EditorHandle`], apply the UDL's style palette to Scintilla's
//! 24 named style indices (`SCE_USER_STYLE_*`), and run the
//! m1c-2 tokeniser against a byte range to paint via
//! `SCI_STARTSTYLING` / `SCI_SETSTYLING`.
//!
//! Kept in its own module so the `ui_win32` lib.rs isn't further
//! bloated by ~200 lines of container-lexer-specific logic and
//! so the m1c-3b-2 menu-integration commit can extend the same
//! primitives cleanly.

use codepp_editor::EditorHandle;
use codepp_scintilla_sys::SCI_GETLENGTH;
use codepp_udl::{Tokeniser, UdlCompiledRules, UdlStyleSlot};

/// Font-style bitfield used by UDL `<WordsStyle fontStyle="...">`
/// attributes. Bit 0 = bold, bit 1 = italic, bit 2 = underline.
/// Matches N++'s own encoding.
const UDL_FONT_STYLE_BOLD: u8 = 0b0000_0001;
const UDL_FONT_STYLE_ITALIC: u8 = 0b0000_0010;
const UDL_FONT_STYLE_UNDERLINE: u8 = 0b0000_0100;

/// Convert a UDL `RRGGBB` hex colour (24-bit, red most
/// significant per m1a's [`codepp_udl::UdlStyle::fg_color`]
/// docstring) to Scintilla's `COLORREF` layout (`0x00BBGGRR`,
/// blue most significant). Byte-swap of the low and high
/// octets; the middle green byte and the reserved top byte
/// stay in place.
///
/// Example: `0x8000FF` (bright red-tinged blue in RGB) →
/// `0xFF0080` (COLORREF blue-tinged red).
#[must_use]
const fn udl_rgb_to_colorref(rgb: u32) -> u32 {
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    (b << 16) | (g << 8) | r
}

/// Map a UDL `<WordsStyle name="...">` string to the Scintilla
/// style index that matches [`UdlStyleSlot`]'s numeric
/// discriminant. Returns `None` for unrecognised names — a UDL
/// authored against a future N++ version that adds new style
/// names must not panic Code++.
///
/// Names are the exact strings N++ writes into the XML —
/// preserved verbatim from the `<Styles>` block. Case matters:
/// N++ uses ALL CAPS throughout.
#[must_use]
fn udl_style_name_to_index(name: &str) -> Option<u8> {
    let slot = match name {
        "DEFAULT" => UdlStyleSlot::Default,
        "COMMENTS" => UdlStyleSlot::Comment,
        "LINE COMMENTS" => UdlStyleSlot::CommentLine,
        "NUMBERS" => UdlStyleSlot::Number,
        "KEYWORDS1" => UdlStyleSlot::Word1,
        "KEYWORDS2" => UdlStyleSlot::Word2,
        "KEYWORDS3" => UdlStyleSlot::Word3,
        "KEYWORDS4" => UdlStyleSlot::Word4,
        "KEYWORDS5" => UdlStyleSlot::Word5,
        "KEYWORDS6" => UdlStyleSlot::Word6,
        "KEYWORDS7" => UdlStyleSlot::Word7,
        "KEYWORDS8" => UdlStyleSlot::Word8,
        "OPERATORS" => UdlStyleSlot::Operator,
        "FOLDER IN CODE1" => UdlStyleSlot::FolderInCode1,
        "FOLDER IN CODE2" => UdlStyleSlot::FolderInCode2,
        "FOLDER IN COMMENT" => UdlStyleSlot::FolderInComment,
        "DELIMITERS1" => UdlStyleSlot::Delimiter1,
        "DELIMITERS2" => UdlStyleSlot::Delimiter2,
        "DELIMITERS3" => UdlStyleSlot::Delimiter3,
        "DELIMITERS4" => UdlStyleSlot::Delimiter4,
        "DELIMITERS5" => UdlStyleSlot::Delimiter5,
        "DELIMITERS6" => UdlStyleSlot::Delimiter6,
        "DELIMITERS7" => UdlStyleSlot::Delimiter7,
        "DELIMITERS8" => UdlStyleSlot::Delimiter8,
        _ => return None,
    };
    Some(slot as u8)
}

/// Apply the UDL's `<Styles>` palette to Scintilla's per-style
/// fore/back/bold/italic/underline attributes at style indices
/// matching [`UdlStyleSlot`]. Called from `Win32Ui::apply_lang`
/// when the target `LangType` is a UDL id.
///
/// **Does NOT reset styles first.** The caller must invoke
/// `apply_default_styles` (which propagates `STYLE_DEFAULT` to
/// every style via `SCI_STYLECLEARALL`) before this so any
/// prior lexer's per-style colours don't bleed through onto
/// indices the UDL doesn't populate. Same discipline as the
/// existing Lexilla-theme path in `apply_lang`.
pub fn apply_udl_styles(editor: &EditorHandle, styles: &[codepp_udl::UdlStyle]) {
    for style in styles {
        let Some(index) = udl_style_name_to_index(&style.name) else {
            // Unknown WordsStyle name — either a UDL authored
            // against a future N++ version we don't know about,
            // or a hand-edit typo. Log and skip.
            tracing::warn!(
                udl_style = ?style.name,
                "unknown UDL WordsStyle name; skipping style application"
            );
            continue;
        };
        let idx = usize::from(index);
        editor.style_set_fore(idx, udl_rgb_to_colorref(style.fg_color));
        editor.style_set_back(idx, udl_rgb_to_colorref(style.bg_color));
        editor.style_set_bold(idx, (style.font_style & UDL_FONT_STYLE_BOLD) != 0);
        editor.style_set_italic(idx, (style.font_style & UDL_FONT_STYLE_ITALIC) != 0);
        editor.style_set_underline(idx, (style.font_style & UDL_FONT_STYLE_UNDERLINE) != 0);
    }
}

/// Tokenise `text_range` of the editor's document via the
/// pre-compiled UDL rules and paint the resulting style events
/// via `SCI_STARTSTYLING` + `SCI_SETSTYLING`. `range_start` is
/// the byte offset (line-aligned by the caller) where painting
/// begins; `text_range` is the caller-fetched byte content.
///
/// Takes a `&UdlCompiledRules` reference rather than a raw
/// `&UdlDefinition` so this hot path (fired on every
/// `SCN_STYLENEEDED`) doesn't rebuild the keyword-class tables
/// per keystroke — see the type's docstring for the DESIGN.md
/// §8 argument. The Arc lives on the [`codepp_udl::UdlEntry`]
/// and is populated once at [`codepp_udl::UdlRegistry::scan_dir`]
/// time.
///
/// **Caller responsibilities:**
/// - Align `range_start` to a line boundary so restart is safe
///   (mid-line restart would drop delimiter-span context).
/// - Have already applied [`apply_udl_styles`] for this UDL so
///   the paint has visible colours to work with.
/// - Bound `text_range.len()` — the m1c-2 tokeniser is
///   linear-time in input length, but bytes still transit
///   through here on the UI thread.
pub fn paint_udl_range(
    editor: &EditorHandle,
    rules: &UdlCompiledRules,
    range_start: usize,
    text_range: &[u8],
) {
    let tokeniser = Tokeniser::new(rules);
    let events = tokeniser.tokenise(text_range);
    editor.start_styling(range_start);
    for event in &events {
        let length = event.end - event.start;
        editor.set_styling(length, event.slot as u8);
    }
}

/// Line-aligned byte range covering `[endStyled, target]`
/// suitable for a container-lexer restyling pass. `target` is
/// the notification position from `SCN_STYLENEEDED`; the
/// returned range extends from the start of the line
/// containing `endStyled` through the end of the line
/// containing `target` (plus a one-line margin), clamped to
/// document length.
///
/// Returns `(range_start, range_end)`. Both are byte offsets;
/// callers subtract to get the length to pass to
/// [`EditorHandle::get_range_bytes`].
///
/// Line alignment is the load-bearing discipline: mid-line
/// restart would produce wrong styling for any comment /
/// delimiter span that crosses the restart boundary, since
/// the tokeniser has no memory of "was I inside a multi-line
/// span?" without an explicit restart-state mechanism (which
/// is deferred to m1c-3b polish — track per-line initial
/// state via `SCI_SETLINESTATE`).
#[must_use]
pub fn line_aligned_range(editor: &EditorHandle, target: usize) -> (usize, usize) {
    let end_styled = editor.get_end_styled();
    let start_line = editor.line_from_position(end_styled);
    // `position_from_line` returns `None` only for lines strictly
    // past the last one — impossible here since `start_line` came
    // out of `line_from_position` which clamps.
    let range_start = editor.position_from_line(start_line).unwrap_or(0);
    let target_line = editor.line_from_position(target);
    // Add one line of margin so the tokeniser sees a bit past
    // the requested end — helps multi-line spans get a
    // consistent boundary. `position_from_line(last+1)` returns
    // `None`; fall back to document length in that case.
    let doc_len = editor.send(SCI_GETLENGTH, 0, 0).max(0) as usize;
    let range_end = editor
        .position_from_line(target_line + 1)
        .unwrap_or(doc_len)
        .min(doc_len);
    (range_start, range_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_to_colorref_swaps_first_and_last_byte() {
        // `0x8000FF` → R=0x80, G=0x00, B=0xFF → COLORREF
        // 0x00BBGGRR = 0x00FF0080.
        assert_eq!(udl_rgb_to_colorref(0x0080_00FF), 0x00FF_0080);
        // White stays white; black stays black; palindromic
        // greys stay palindromic (0xBEBEBE, 0x808080, etc.).
        assert_eq!(udl_rgb_to_colorref(0x00FF_FFFF), 0x00FF_FFFF);
        assert_eq!(udl_rgb_to_colorref(0x0000_0000), 0x0000_0000);
        assert_eq!(udl_rgb_to_colorref(0x00BE_BEBE), 0x00BE_BEBE);
        assert_eq!(udl_rgb_to_colorref(0x0080_8080), 0x0080_8080);
    }

    #[test]
    fn style_name_maps_to_slot_discriminants() {
        // Regression pin: `udl_style_name_to_index` must return
        // exactly the discriminant `UdlStyleSlot` uses so the
        // paint sees the correct index. Any drift between the
        // enum discriminant and this map is a paint bug that
        // silently mis-colours everything.
        assert_eq!(udl_style_name_to_index("DEFAULT"), Some(0));
        assert_eq!(udl_style_name_to_index("COMMENTS"), Some(1));
        assert_eq!(udl_style_name_to_index("LINE COMMENTS"), Some(2));
        assert_eq!(udl_style_name_to_index("NUMBERS"), Some(3));
        assert_eq!(udl_style_name_to_index("KEYWORDS1"), Some(4));
        assert_eq!(udl_style_name_to_index("KEYWORDS8"), Some(11));
        assert_eq!(udl_style_name_to_index("OPERATORS"), Some(12));
        assert_eq!(udl_style_name_to_index("FOLDER IN CODE1"), Some(13));
        assert_eq!(udl_style_name_to_index("FOLDER IN CODE2"), Some(14));
        assert_eq!(udl_style_name_to_index("FOLDER IN COMMENT"), Some(15));
        assert_eq!(udl_style_name_to_index("DELIMITERS1"), Some(16));
        assert_eq!(udl_style_name_to_index("DELIMITERS8"), Some(23));
    }

    #[test]
    fn style_name_returns_none_for_unknown() {
        // Future N++ UDL versions might add new WordsStyle
        // names. Don't panic; return None so
        // `apply_udl_styles` logs and skips gracefully.
        assert_eq!(udl_style_name_to_index(""), None);
        assert_eq!(udl_style_name_to_index("keywords1"), None); // wrong case
        assert_eq!(udl_style_name_to_index("KEYWORDS9"), None); // out of range
        assert_eq!(udl_style_name_to_index("SOMETHING NEW"), None);
    }

    #[test]
    fn font_style_bits_match_notepad_plus_plus_encoding() {
        // Pin the bit assignments against the markdown fixture's
        // observed values. Fixture uses fontStyle="2" (italic)
        // for COMMENTS, "1" (bold) for LINE COMMENTS, "3"
        // (bold+italic) for KEYWORDS3, etc.
        assert_eq!(UDL_FONT_STYLE_BOLD, 1);
        assert_eq!(UDL_FONT_STYLE_ITALIC, 2);
        assert_eq!(UDL_FONT_STYLE_UNDERLINE, 4);
    }
}
