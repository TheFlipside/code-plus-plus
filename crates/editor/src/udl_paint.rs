//! UDL container-lexer painting helpers (shared by every UI backend).
//!
//! Phase 4.6 m1c-3b-1 (Win32) established these; Phase 5 lifted them out
//! of `ui_win32` into `codepp_editor` so the GTK (and future Cocoa)
//! backends share one copy — the same move the Phase 5 m2a lexer-theme
//! table made. Nothing here is platform-specific: every operation goes
//! through [`EditorHandle`] (the cross-platform direct-call wrapper),
//! `codepp_scintilla_sys`, and the headless `codepp_udl` tokeniser.
//!
//! Given a [`codepp_udl::UdlDefinition`]'s style palette and its
//! pre-compiled rules, these apply the UDL's colours to Scintilla's 24
//! named style indices (`SCE_USER_STYLE_*`) and run the tokeniser against
//! a byte range to paint via `SCI_STARTSTYLING` / `SCI_SETSTYLING`.
//!
//! Two orchestration entry points wrap the leaf helpers so both backends
//! share the security-relevant paint-cap logic rather than duplicating
//! it: [`apply_udl_lang`] (put Scintilla in container-lexer mode + apply
//! the palette + an initial bounded paint) and [`paint_style_needed`]
//! (the capped `SCN_STYLENEEDED` restyle). The platform-specific parts —
//! the registry lookup and the backend's borrow discipline — stay in
//! each UI crate.

use crate::EditorHandle;
use codepp_scintilla_sys::SCI_GETLENGTH;
use codepp_udl::{Tokeniser, UdlCompiledRules, UdlStyleSlot};

/// Cap on the initial synchronous paint in [`apply_udl_lang`], and on the
/// per-notification paint in [`paint_style_needed`]. 64 KiB comfortably
/// covers a viewport-sized region and the visible window of a multi-MB
/// file; the tokeniser is linear-time, so this bounds the wall clock
/// deterministically and keeps the UI inside DESIGN.md §8's budget. The
/// rest fills in via further `SCN_STYLENEEDED` notifications as the user
/// scrolls. It is also the reviewer-required denial-of-service defence: a
/// plugin or a stray `SCI_COLOURISE(0, -1)` can request whole-document
/// styling, which Scintilla delivers synchronously on the UI thread.
const MAX_PAINT_RANGE: usize = 64 * 1024;

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
/// matching [`UdlStyleSlot`].
///
/// **Does NOT reset styles first.** The caller must invoke
/// [`crate::theme::apply_default_styles`] (which propagates
/// `STYLE_DEFAULT` to every style via `SCI_STYLECLEARALL`) before this so
/// any prior lexer's per-style colours don't bleed through onto indices
/// the UDL doesn't populate. [`apply_udl_lang`] does this for you.
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
    // Defence-in-depth: the tokeniser is first-party but its rules are
    // compiled from untrusted `userDefineLangs/*.xml`. `SCI_SETSTYLING`
    // advances a cursor by `length` bytes, so events must stay contiguous
    // and within the fetched range. `saturating_sub` turns a malformed
    // `end < start` into a harmless zero-length step instead of an
    // underflow (which would wrap in release), and the `remaining` budget
    // stops the cursor ever styling past `range_start + text_range.len()`.
    let mut remaining = text_range.len();
    for event in &events {
        if remaining == 0 {
            break;
        }
        let length = event.end.saturating_sub(event.start).min(remaining);
        editor.set_styling(length, event.slot as u8);
        remaining -= length;
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

/// End offset for a bounded, line-aligned paint that begins at `start`,
/// covers at most `max_range` bytes, and never exceeds `hard_end` (document
/// length, or a caller-supplied range end).
///
/// It extends to the start of the line *after* the byte cap — a one-line
/// overshoot so a delimiter / comment span straddling the cap still gets
/// consistent context — but **bounds that overshoot at one extra
/// `max_range`**. The bound is load-bearing: `position_from_line(line + 1)`
/// returns `None` on the last line and a possibly-distant offset on a very
/// long line, so an unbounded "extend to end of line" defeats the cap for a
/// single line longer than `max_range` (a minified JS/CSS, a base64 blob, a
/// file with no trailing newline), dragging the paint over the whole
/// remainder synchronously on the UI thread and violating DESIGN.md §8's
/// UI-never-blocks constraint — the very denial-of-service the cap exists to
/// prevent. With the bound, every call paints at most `2 * max_range`; a
/// pathological long line simply fills in over several notifications.
///
/// Pure over the two positional lookups so the cap-preservation invariant is
/// unit-testable without a live Scintilla control.
fn capped_end(
    start: usize,
    max_range: usize,
    hard_end: usize,
    line_from_position: impl Fn(usize) -> usize,
    position_from_line: impl Fn(usize) -> Option<usize>,
) -> usize {
    let cap = start.saturating_add(max_range).min(hard_end);
    let cap_line = line_from_position(cap);
    // Extend to the next line boundary (`None` on the last line → stay at
    // the byte cap), then clamp the overshoot to one extra `max_range` and
    // finally to `hard_end`.
    position_from_line(cap_line + 1)
        .unwrap_or(cap)
        .min(cap.saturating_add(max_range))
        .min(hard_end)
}

/// Put Scintilla into container-lexer mode for a UDL buffer, install the
/// UDL's style palette, and paint an initial bounded region. The
/// backend-agnostic core of what each UI's `apply_lang` does once its
/// registry lookup has produced the UDL's `styles` and compiled `rules`.
///
/// **Why the initial paint is required and bounded.** `clear_lexer()`
/// swaps only Scintilla's lexer pointer; it does not reset the
/// `endStyled` cursor (verified against
/// `vendor/scintilla/src/Document.cxx` — `SCI_SETILEXER(0, 0)` leaves
/// `endStyled` alone). Without an initial paint the previously-styled
/// bytes keep old style indices that now point at the freshly-cleared
/// palette, rendering as visually-empty styling until the user scrolls.
/// So an initial paint is needed — but a full-document paint would
/// violate DESIGN.md §8's UI-never-blocks constraint on multi-MB files,
/// so it is capped at [`MAX_PAINT_RANGE`]; the rest fills in via
/// `SCN_STYLENEEDED`.
///
/// The caller must have reset styles first — this does that via
/// [`crate::theme::apply_default_styles`] so any prior lexer's per-style
/// colours don't bleed onto indices the UDL doesn't populate.
pub fn apply_udl_lang(
    editor: &EditorHandle,
    styles: &[codepp_udl::UdlStyle],
    rules: &UdlCompiledRules,
) {
    editor.clear_lexer();
    crate::theme::apply_default_styles(editor);
    apply_udl_styles(editor, styles);
    let doc_len = editor.send(SCI_GETLENGTH, 0, 0).max(0) as usize;
    // Line-aligned, cap-preserving end (see `capped_end`): bounds the
    // initial synchronous paint at ~64 KiB even for a huge single line.
    let paint_end = capped_end(
        0,
        MAX_PAINT_RANGE,
        doc_len,
        |p| editor.line_from_position(p),
        |l| editor.position_from_line(l),
    );
    if paint_end > 0 {
        if let Some(bytes) = editor.get_range_bytes(0, paint_end) {
            paint_udl_range(editor, rules, 0, &bytes);
        }
    }
}

/// Paint the range Scintilla asked for in an `SCN_STYLENEEDED`
/// notification, capped at [`MAX_PAINT_RANGE`]. `target` is the
/// notification's `position` field — the byte offset up to which
/// Scintilla wants styling. The backend-agnostic core of each UI's
/// `SCN_STYLENEEDED` handler once its registry lookup has produced the
/// compiled `rules`.
///
/// Each `SCI_SETSTYLING` in [`paint_udl_range`] advances Scintilla's
/// `endStyled`, so any bytes past the cap trigger a fresh
/// `SCN_STYLENEEDED` on the next paint iteration — the range fills in
/// incrementally rather than blocking the UI thread on a whole-document
/// request.
///
/// **Backend caller responsibility:** each `SCI_SETSTYLING` synchronously
/// fires `SCN_MODIFIED(ChangeStyle)`, which re-enters the host's
/// notification dispatch. The caller must therefore have released any
/// exclusive borrow of its window state (Win32: dropped `&WindowState`;
/// GTK: returned from the `with_state` closure) before calling this.
pub fn paint_style_needed(editor: &EditorHandle, rules: &UdlCompiledRules, target: usize) {
    let (range_start, range_end) = line_aligned_range(editor, target);
    if range_end <= range_start {
        // Nothing to style (empty range or one that inverted under
        // document-length clamping). Skip cleanly.
        return;
    }
    // Line-aligned, cap-preserving end (see `capped_end`): bounds this
    // per-notification paint at ~64 KiB regardless of line length, so a
    // whole-document `SCN_STYLENEEDED` or a huge single line can't freeze
    // the UI thread.
    let capped_range_end = capped_end(
        range_start,
        MAX_PAINT_RANGE,
        range_end,
        |p| editor.line_from_position(p),
        |l| editor.position_from_line(l),
    );
    let range_len = capped_range_end - range_start;
    let Some(bytes) = editor.get_range_bytes(range_start, range_len) else {
        return;
    };
    paint_udl_range(editor, rules, range_start, &bytes);
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

    const CAP: usize = 64 * 1024;

    #[test]
    fn capped_end_line_aligns_a_normal_multiline_paint() {
        // Lines every 100 bytes: the cap rounds up to the next line
        // boundary (a small overshoot), staying well under 2×cap.
        let lfp = |p: usize| p / 100;
        let pfl = |l: usize| Some(l * 100);
        let end = capped_end(0, CAP, 10_000_000, lfp, pfl);
        assert_eq!(end, (CAP / 100 + 1) * 100);
        assert!(end <= 2 * CAP);
    }

    #[test]
    fn capped_end_preserves_cap_on_a_huge_last_line() {
        // A single line longer than the cap: `position_from_line(1)` is
        // `None` (last line), so the end falls back to the byte cap — NOT
        // the document length. This is the denial-of-service regression the
        // fix closes (the old `.unwrap_or(doc_len)` painted the whole file).
        let lfp = |_p: usize| 0usize;
        let pfl = |_l: usize| None;
        let end = capped_end(0, CAP, 200 * 1024 * 1024, lfp, pfl);
        assert_eq!(end, CAP, "cap must be preserved, not extended to doc end");
    }

    #[test]
    fn capped_end_bounds_a_huge_non_last_line() {
        // A huge line 0 followed by more lines: `position_from_line(1)`
        // returns a distant offset. The line-completion overshoot must
        // still be bounded at one extra max_range, not dragged to the huge
        // line's far end (which `.unwrap_or(capped_end)` alone would miss).
        let huge = 100 * 1024 * 1024;
        let lfp = move |p: usize| usize::from(p >= huge);
        let pfl = move |l: usize| if l == 0 { Some(0) } else { Some(huge) };
        let end = capped_end(0, CAP, 200 * 1024 * 1024, lfp, pfl);
        assert_eq!(end, 2 * CAP, "overshoot must be bounded at 2x max_range");
    }

    #[test]
    fn capped_end_paints_whole_small_document() {
        // Document smaller than the cap: paint all of it.
        let lfp = |p: usize| p / 100;
        let pfl = |l: usize| if l <= 3 { Some(l * 100) } else { None };
        assert_eq!(capped_end(0, CAP, 300, lfp, pfl), 300);
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
