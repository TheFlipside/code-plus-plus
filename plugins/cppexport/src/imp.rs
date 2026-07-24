//! Entry-point implementations for cppexport.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! Five menu items, an Export-to-file + Copy-to-clipboard pair per
//! format plus a combined "copy all":
//!   * **Export to HTML... / Export to RTF...** — render the styled
//!     buffer, then hand the bytes to the host's Save-As service
//!     ([`sdk::export_save_dialog`]).
//!   * **Copy HTML / RTF to Clipboard** — hand the bytes to the host's
//!     clipboard service ([`sdk::set_clipboard`]) tagged with the
//!     format.
//!   * **Copy All Formats to Clipboard** — plain + HTML + RTF in one
//!     call so the pasting app picks the richest it understands.
//!
//! The two host services are what keep this plugin **portable**: it
//! never touches the platform's Save-As dialog or clipboard directly.
//! Each backend (Win32 / GTK / Cocoa) does the OS-specific dialog,
//! file write, and clipboard packaging (including the Win32 `CF_HTML`
//! byte-offset envelope). See `codepp_plugin_host::codepp_ext`.
//!
//! Style extraction is straightforward but per-character: walk the
//! buffer with `SCI_GETSTYLEDTEXTFULL`, collapse adjacent same-style
//! runs, then query `SCI_STYLEGET{FORE,BACK,BOLD,ITALIC,SIZE,FONT}`
//! for each unique style. The output uses class-based CSS so a
//! 5000-line file with 8 active styles emits 8 CSS rules and one
//! `<span>` per run, not one per character.

#![cfg(any(target_os = "windows", target_os = "linux"))]
// HTML emission builds output via repeated `out += &format!(...)`
// — clippy prefers `write!(out, ...).unwrap()`, but for HTML the `+=`
// form reads more naturally and the builder closures don't gain
// anything from going through the `Write` trait. Allowed at module
// scope to keep the emitter readable.
//
// `manual_let_else` is also allowed: the FFI-validation `match`
// patterns in this file (resolve handle / read length / decode
// styled-text) read more clearly as explicit matches than as
// `let ... else` because each arm carries a different error
// path (early return, log + return, ignore, …).
#![allow(clippy::format_push_string, clippy::manual_let_else)]

use codepp_plugin_sdk::{
    self as sdk, FuncItem, Hwnd, NppData, SCNotification, SyncCell, CLIP_FORMAT_HTML,
    CLIP_FORMAT_PLAIN, CLIP_FORMAT_RTF, EXPORT_KIND_HTML, EXPORT_KIND_RTF,
};

// Plugin-specific Scintilla messages — the buffer-text/style
// queries and the per-style attribute reads the HTML/RTF export
// needs. The SDK only ships the selection-replacement subset every
// plugin shares; per-style introspection is unique to cppexport.
const SCI_GETLENGTH: u32 = 2006;
const SCI_GETSTYLEDTEXTFULL: u32 = 2778;
const SCI_STYLEGETFORE: u32 = 2481;
const SCI_STYLEGETBACK: u32 = 2482;
const SCI_STYLEGETBOLD: u32 = 2483;
const SCI_STYLEGETITALIC: u32 = 2484;
const SCI_STYLEGETSIZE: u32 = 2485;
const SCI_STYLEGETFONT: u32 = 2486;
const STYLE_DEFAULT: u32 = 32;

/// Mirror of Scintilla's `Sci_CharacterRangeFull` (`Sci_Position`
/// = `ptrdiff_t`, so two pointer-sized signed integers).
#[repr(C)]
struct SciCharacterRangeFull {
    cp_min: isize,
    cp_max: isize,
}

/// Mirror of Scintilla's `Sci_TextRangeFull`. Used as the
/// `lparam` for `SCI_GETSTYLEDTEXTFULL`. Scintilla fills the
/// buffer with interleaved (text byte, style byte) pairs over
/// the requested range.
#[repr(C)]
struct SciTextRangeFull {
    chrg: SciCharacterRangeFull,
    lpstr_text: *mut u8,
}

const PLUGIN_NAME: [u16; 7] = make_plugin_name();

const fn make_plugin_name() -> [u16; 7] {
    // "Export\0" — 6 ASCII chars + NUL.
    let mut buf = [0u16; 7];
    let bytes = b"Export";
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

static FUNCS: SyncCell<[FuncItem; 5]> = SyncCell::new([
    FuncItem {
        item_name: sdk::menu_label(b"Export to HTML..."),
        p_func: Some(cmd_export_html),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Copy HTML to Clipboard"),
        p_func: Some(cmd_copy_html),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Export to RTF..."),
        p_func: Some(cmd_export_rtf),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Copy RTF to Clipboard"),
        p_func: Some(cmd_copy_rtf),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Copy All Formats to Clipboard"),
        p_func: Some(cmd_copy_all),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
]);

#[no_mangle]
pub extern "C" fn setInfo(data: NppData) {
    sdk::store_handles(data);
}

#[no_mangle]
pub extern "C" fn getName() -> *const u16 {
    PLUGIN_NAME.as_ptr()
}

#[no_mangle]
pub extern "C" fn getFuncsArray(nb: *mut i32) -> *mut FuncItem {
    if !nb.is_null() {
        // SAFETY: per the ABI, `nb` is a valid out-pointer the host
        // owns for the duration of this call.
        unsafe { *nb = 5 };
    }
    FUNCS.get().cast::<FuncItem>()
}

#[no_mangle]
pub extern "C" fn beNotified(_notification: *const SCNotification) {}

#[no_mangle]
pub extern "C" fn messageProc(_msg: u32, _wparam: usize, _lparam: isize) -> isize {
    0
}

#[no_mangle]
pub extern "C" fn isUnicode() -> i32 {
    1
}

// ---- Scintilla helpers ----

/// Pull the entire buffer's text and per-byte style indices in a
/// single `SCI_GETSTYLEDTEXTFULL` round-trip. Returns
/// `(text_bytes, style_bytes)` of equal length.
///
/// **Performance:** the prior implementation called `SCI_GETSTYLEAT`
/// once per byte — N round-trips for an N-byte buffer. On a 200KB
/// source file that's 200,000 `SendMessage` calls; cppexport users
/// reported export-button latency. `SCI_GETSTYLEDTEXTFULL` returns
/// the same data in one call by writing alternating
/// (`text_byte`, `style_byte`) pairs into a caller-supplied buffer.
///
/// **Layout:** Scintilla writes `2 * (cp_max - cp_min) + 2` bytes —
/// one text byte plus one style byte per character in the range,
/// followed by a trailing NUL pair. We split the result by even/odd
/// indices into the two output streams.
fn collect_text_and_styles(sci: Hwnd, len: usize) -> (Vec<u8>, Vec<u8>) {
    if sci.is_null() || len == 0 {
        return (Vec::new(), Vec::new());
    }
    // Allocate the interleaved buffer. `len.checked_mul(2)` guards
    // against a (theoretical) document so large that doubling
    // overflows usize — defense in depth, since the document size
    // can't exceed `isize::MAX` on any realistic target.
    let alloc = match len.checked_mul(2).and_then(|n| n.checked_add(2)) {
        Some(n) => n,
        None => return (Vec::new(), Vec::new()),
    };
    // Convert `len` to `isize` for the range's `cp_max` field
    // explicitly rather than via an `as` cast — `len > isize::MAX`
    // (already implicit-capped by the `checked_mul(2)` guard above,
    // but worth pinning) would otherwise cast to a negative value
    // and Scintilla would silently treat the range as empty.
    let cp_max = match isize::try_from(len) {
        Ok(n) => n,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let mut buf = vec![0u8; alloc];
    let buf_ptr = buf.as_mut_ptr();

    let mut range = SciTextRangeFull {
        chrg: SciCharacterRangeFull { cp_min: 0, cp_max },
        lpstr_text: buf_ptr,
    };
    // SAFETY: `range` is a valid `Sci_TextRangeFull` for the
    // duration of the call. Scintilla writes through
    // `range.lpstr_text` at most `2 * (cp_max - cp_min) + 2 =
    // alloc` bytes — exactly the size we allocated. Synchronous
    // UI-thread call, same no-TOCTOU invariant as the SDK's
    // `get_selection_bytes` (no other code can mutate the buffer
    // between the SCI_GETLENGTH that produced `len` and the fill
    // call here).
    unsafe {
        sdk::SendMessageW(sci, SCI_GETSTYLEDTEXTFULL, 0, &raw mut range as isize);
    }

    // Split interleaved (text, style) pairs into two byte streams.
    // `chunks_exact(2).take(len)` handles the trailing NUL pair
    // implicitly — we drop it by only consuming `len` chunks.
    let mut text = Vec::with_capacity(len);
    let mut styles = Vec::with_capacity(len);
    for pair in buf.chunks_exact(2).take(len) {
        text.push(pair[0]);
        styles.push(pair[1]);
    }
    (text, styles)
}

/// Pull the per-style attributes (foreground / background / bold /
/// italic / size / font) Scintilla needs to render a span. Result is
/// the snapshot at the time of the call; a re-styling that happens
/// during export would not be reflected, but export runs on the UI
/// thread synchronously so re-styling can't happen mid-export.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StyleAttrs {
    fore_rgb: u32,
    back_rgb: u32,
    bold: bool,
    italic: bool,
    size_pts: i32,
    font_name: String,
}

fn query_style_attrs(sci: Hwnd, style: u8) -> StyleAttrs {
    // SAFETY: SCI_STYLEGET* take wparam=style; no pointer arguments.
    // Return values are RGB (0x00BBGGRR) or boolean (0/1) or pt size.
    let style = style as usize;
    let fore_rgb = unsafe { sdk::SendMessageW(sci, SCI_STYLEGETFORE, style, 0) } as u32;
    let back_rgb = unsafe { sdk::SendMessageW(sci, SCI_STYLEGETBACK, style, 0) } as u32;
    let bold = unsafe { sdk::SendMessageW(sci, SCI_STYLEGETBOLD, style, 0) } != 0;
    let italic = unsafe { sdk::SendMessageW(sci, SCI_STYLEGETITALIC, style, 0) } != 0;
    let size_pts = unsafe { sdk::SendMessageW(sci, SCI_STYLEGETSIZE, style, 0) } as i32;
    let font_name = query_style_font(sci, style);
    StyleAttrs {
        fore_rgb,
        back_rgb,
        bold,
        italic,
        size_pts,
        font_name,
    }
}

/// `SCI_STYLEGETFONT(style, char *font)` writes a NUL-terminated
/// ASCII (or UTF-8) font name. Two-phase like `SCI_GETSELTEXT`: pass
/// null first to get length, then alloc and call again. Empty name
/// (Scintilla returns 0) means "use the default font" — we surface
/// the empty string and the consumer falls back to the body's CSS.
fn query_style_font(sci: Hwnd, style: usize) -> String {
    // SAFETY: passing wparam=style, lparam=0 asks for the length only;
    // Scintilla writes nothing through any pointer.
    let len = unsafe { sdk::SendMessageW(sci, SCI_STYLEGETFONT, style, 0) };
    if len <= 0 {
        return String::new();
    }
    let len_us = match usize::try_from(len) {
        Ok(n) => n,
        Err(_) => return String::new(),
    };
    let alloc = match len_us.checked_add(1) {
        Some(n) => n,
        None => return String::new(),
    };
    let mut buf = vec![0u8; alloc];
    // SAFETY: `buf.as_mut_ptr()` is valid for `alloc` bytes; Scintilla
    // writes the font name plus NUL terminator there.
    unsafe {
        sdk::SendMessageW(sci, SCI_STYLEGETFONT, style, buf.as_mut_ptr() as isize);
    }
    buf.truncate(len_us);
    // Lossy decode rather than strict — Scintilla can return font
    // names in the active codepage on legacy systems (Latin-1
    // "Consolas-foo" with a Latin-1 byte slipping through, for
    // example). Strict `from_utf8` would discard the *whole* name
    // on the first invalid byte; lossy substitutes the bad bytes
    // with U+FFFD which `sanitize_font_name` later filters out,
    // so a partly-valid name like "Consolas-XXX" survives as
    // "Consolas-" rather than collapsing entirely to monospace.
    String::from_utf8_lossy(&buf).into_owned()
}

// ---- HTML construction (pure-Rust core, fully testable) ----

/// One contiguous run of identically-styled bytes from the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StyleRun {
    style: u8,
    bytes: Vec<u8>,
}

/// Group a `(text_bytes, style_bytes)` pair into runs — adjacent
/// positions with the same style index collapse into a single
/// `StyleRun`. Returns an empty vec for empty input.
fn group_runs(text: &[u8], styles: &[u8]) -> Vec<StyleRun> {
    let n = text.len().min(styles.len());
    let mut runs = Vec::new();
    if n == 0 {
        return runs;
    }
    let mut start = 0;
    for i in 1..n {
        if styles[i] != styles[start] {
            runs.push(StyleRun {
                style: styles[start],
                bytes: text[start..i].to_vec(),
            });
            start = i;
        }
    }
    runs.push(StyleRun {
        style: styles[start],
        bytes: text[start..n].to_vec(),
    });
    runs
}

/// Escape one byte sequence (interpreted as UTF-8 with lossy
/// fallback on invalid sequences) into HTML-safe form: `&` → `&amp;`,
/// `<` → `&lt;`, `>` → `&gt;`, `"` → `&quot;`. Other characters pass
/// through verbatim — the output is wrapped in `<span>` whose CSS
/// `white-space: pre` preserves whitespace and newlines as-is.
fn escape_html_into(out: &mut String, bytes: &[u8]) {
    let s = String::from_utf8_lossy(bytes);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
}

/// Strip every character outside the conservative allowlist
/// `[A-Za-z0-9 _-]` from a font name so an attacker-supplied value
/// can't escape the surrounding `<style>` block by embedding
/// `</style><script>…`. The font name flows from `SCI_STYLEGETFONT`,
/// which any plugin (including a third-party DLL co-resident in the
/// plugins directory) can mutate via `SCI_STYLESETFONT`. Output-side
/// HTML escaping is insufficient because `</style>` inside a CSS
/// string still terminates the style element. An allowlist is the
/// only correct fix; every real-world font-family name is covered.
fn sanitize_font_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .collect()
}

/// Format a Scintilla-style RGB integer (`0x00BBGGRR`) as a CSS
/// `#RRGGBB` string. Scintilla's color order is little-endian-like
/// (the low byte is R, then G, then B); CSS wants the canonical
/// big-endian "RRGGBB" hex form.
fn rgb_to_css(rgb: u32) -> String {
    let r = rgb & 0xff;
    let g = (rgb >> 8) & 0xff;
    let b = (rgb >> 16) & 0xff;
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// Assemble the final HTML document.
///
/// Output structure: a minimal `<!DOCTYPE html>` page with a single
/// `<style>` block containing one rule per active style and a body
/// rule pinning `white-space: pre` (so newlines and runs of spaces
/// from the source survive). The body itself is one `<span>` per
/// `StyleRun`. The default style (`STYLE_DEFAULT` = 32) is folded
/// into the body's CSS, so a run of all-default-style text emits
/// `<span class="s32">…</span>` with the same colors the body has —
/// slightly redundant but keeps `group_runs` simple.
fn build_html(runs: &[StyleRun], style_attrs: &[(u8, StyleAttrs)]) -> String {
    let default = style_attrs
        .iter()
        .find(|(s, _)| u32::from(*s) == STYLE_DEFAULT)
        .map_or(
            StyleAttrs {
                fore_rgb: 0x00_00_00_00,
                back_rgb: 0x00_FF_FF_FF,
                bold: false,
                italic: false,
                size_pts: 11,
                font_name: String::from("Consolas"),
            },
            |(_, a)| a.clone(),
        );

    let mut html =
        String::with_capacity(1024 + runs.iter().map(|r| r.bytes.len() * 2).sum::<usize>());
    html.push_str("<!DOCTYPE html>\n<html>\n<head>\n");
    html.push_str("<meta charset=\"UTF-8\">\n");
    html.push_str("<title>Exported from Code++</title>\n");
    html.push_str("<style>\n");

    // Body rule: default font / size / colors. `white-space: pre`
    // makes spaces and newlines render literally, no `<br>`s needed.
    html.push_str("body {\n");
    let sanitized_font = sanitize_font_name(&default.font_name);
    let body_font = if sanitized_font.is_empty() {
        "monospace"
    } else {
        sanitized_font.as_str()
    };
    html.push_str(&format!("  font-family: {body_font:?}, monospace;\n"));
    html.push_str(&format!("  font-size: {}pt;\n", default.size_pts));
    html.push_str(&format!("  color: {};\n", rgb_to_css(default.fore_rgb)));
    html.push_str(&format!(
        "  background-color: {};\n",
        rgb_to_css(default.back_rgb)
    ));
    html.push_str("  white-space: pre;\n");
    html.push_str("}\n");

    // Per-style rules. Skip the body-default class so our HTML
    // doesn't duplicate the body styling; non-default styles get
    // their own class.
    for (style, attrs) in style_attrs {
        if u32::from(*style) == STYLE_DEFAULT {
            continue;
        }
        html.push_str(&format!(".s{style} {{ "));
        html.push_str(&format!("color: {};", rgb_to_css(attrs.fore_rgb)));
        if attrs.back_rgb != default.back_rgb {
            html.push_str(&format!(
                " background-color: {};",
                rgb_to_css(attrs.back_rgb)
            ));
        }
        if attrs.bold {
            html.push_str(" font-weight: bold;");
        }
        if attrs.italic {
            html.push_str(" font-style: italic;");
        }
        html.push_str(" }\n");
    }

    html.push_str("</style>\n</head>\n<body>");

    for run in runs {
        if u32::from(run.style) == STYLE_DEFAULT {
            // No class needed — body styling already applies. Just
            // wrap in <span> for symmetry so a future restyling pass
            // can target each run uniformly.
            html.push_str("<span>");
        } else {
            html.push_str(&format!("<span class=\"s{}\">", run.style));
        }
        escape_html_into(&mut html, &run.bytes);
        html.push_str("</span>");
    }

    html.push_str("</body>\n</html>\n");
    html
}

// ---- RTF construction (pure-Rust core, fully testable) --------

/// Escape a single Unicode `char` into RTF body syntax, appending
/// the result to `out`.
///
/// RTF body rules:
///   * Backslash, `{`, and `}` are control characters and must be
///     escaped as `\\`, `\{`, `\}`.
///   * `\n` becomes `\par\n` so the receiving editor renders a
///     paragraph break (and the source RTF stays human-readable).
///     `\r` is dropped — `\r\n` collapses to `\par`, lone `\r` is
///     ignored because most modern source files use LF or CRLF
///     and emitting a stray `\par` for `\r` alone would double
///     the paragraph count on Mac-classic line endings.
///   * `\t` becomes `\tab ` (with a trailing space terminator —
///     RTF parses control words by reading until a non-letter).
///   * Printable ASCII (0x20–0x7E) other than the above goes
///     through literally.
///   * Everything else is encoded via `\uN?`, where N is the UTF-16
///     code unit interpreted as a signed 16-bit integer (RTF's
///     wire format) and `?` is the fallback character for legacy
///     readers that don't recognise `\u`. Non-BMP code points
///     emit a UTF-16 surrogate pair (two `\uN?`s).
fn rtf_escape_char_into(out: &mut String, c: char) {
    match c {
        '\\' => out.push_str("\\\\"),
        '{' => out.push_str("\\{"),
        '}' => out.push_str("\\}"),
        '\n' => out.push_str("\\par\n"),
        '\r' => {} // dropped — see doc comment.
        '\t' => out.push_str("\\tab "),
        c if (0x20..=0x7E).contains(&(c as u32)) => out.push(c),
        c => {
            // Non-ASCII: emit each UTF-16 code unit as `\uN?` with
            // N interpreted as a signed 16-bit integer per the RTF
            // spec. `c.encode_utf16(&mut buf)` returns a slice of
            // 1 unit (BMP) or 2 units (surrogate pair).
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                let signed = *unit as i16;
                out.push_str(&format!("\\u{signed}?"));
            }
        }
    }
}

/// Assemble a complete RTF document from styled runs and the
/// per-style attribute table.
///
/// Output structure: standard `{\rtf1\ansi\deff0` preamble, then a
/// `{\fonttbl}` group with one font (the default style's), then a
/// `{\colortbl}` group with one entry per unique foreground colour
/// across all styles (de-duplicated so identical colours share an
/// index), then a default font-size declaration, then the body —
/// each run prefixed with `\cfN\bN\iN ` to set its colour and
/// bold/italic state. Finishes with `}`.
///
/// The body always sets bold and italic explicitly per-run (rather
/// than relying on RTF's inheritance) so a run boundary doesn't
/// silently carry the previous run's emphasis through.
fn build_rtf(runs: &[StyleRun], style_attrs: &[(u8, StyleAttrs)]) -> String {
    let default = style_attrs
        .iter()
        .find(|(s, _)| u32::from(*s) == STYLE_DEFAULT)
        .map_or(
            StyleAttrs {
                fore_rgb: 0x00_00_00_00,
                back_rgb: 0x00_FF_FF_FF,
                bold: false,
                italic: false,
                size_pts: 11,
                font_name: String::from("Consolas"),
            },
            |(_, a)| a.clone(),
        );

    // Build the colour table. RTF's `\colortbl;` starts with an
    // implicit empty "auto" entry (index 0) before any
    // user-defined colours, so our user-defined indices are
    // 1-based. Use BTreeMap for the dedup-by-key (identical RGBs
    // share an index); deterministic output ordering comes from
    // the upstream `style_attrs` iteration order, which is itself
    // BTreeSet-ordered inside `collect_style_attrs`.
    let mut color_index: std::collections::BTreeMap<u32, usize> = std::collections::BTreeMap::new();
    for (_, attrs) in style_attrs {
        if !color_index.contains_key(&attrs.fore_rgb) {
            let next = color_index.len() + 1;
            color_index.insert(attrs.fore_rgb, next);
        }
    }
    // Ordered insertion: rebuild the colour-table list in
    // assigned-index order so the `\redR\greenG\blueB;` entries
    // line up with the indices we'll emit in `\cfN`.
    let mut colors_ordered: Vec<(u32, usize)> =
        color_index.iter().map(|(rgb, idx)| (*rgb, *idx)).collect();
    colors_ordered.sort_by_key(|(_, idx)| *idx);

    let mut rtf = String::new();
    rtf.push_str("{\\rtf1\\ansi\\deff0\n");

    // Font table — one entry. Sanitize the font name (same
    // allowlist as build_html) so a hostile lexer-set font name
    // can't inject RTF control words via the font-name slot.
    let body_font = sanitize_font_name(&default.font_name);
    let body_font = if body_font.is_empty() {
        "Consolas".to_string()
    } else {
        body_font
    };
    rtf.push_str(&format!("{{\\fonttbl{{\\f0\\fmodern {body_font};}}}}\n"));

    // Color table.
    rtf.push_str("{\\colortbl;");
    for (rgb, _) in &colors_ordered {
        let r = rgb & 0xff;
        let g = (rgb >> 8) & 0xff;
        let b = (rgb >> 16) & 0xff;
        rtf.push_str(&format!("\\red{r}\\green{g}\\blue{b};"));
    }
    rtf.push_str("}\n");

    // Default font size in half-points (RTF convention). Clamp
    // negative values to 0 — `SCI_STYLEGETSIZE` is documented as
    // a positive integer but a hostile lexer could return a
    // negative i32 and `\fs-2` is malformed RTF that some
    // consumers reject with a parse error.
    rtf.push_str(&format!(
        "\\fs{}\n",
        default.size_pts.max(0).saturating_mul(2),
    ));

    // Body — emit each run with explicit colour + bold + italic
    // state, then the escaped run text.
    for run in runs {
        let attrs = style_attrs
            .iter()
            .find(|(s, _)| *s == run.style)
            .map(|(_, a)| a);
        let cf_idx = attrs
            .and_then(|a| color_index.get(&a.fore_rgb))
            .copied()
            .unwrap_or(0);
        let bold = attrs.is_some_and(|a| a.bold);
        let italic = attrs.is_some_and(|a| a.italic);
        rtf.push_str(&format!(
            "\\cf{cf_idx}\\b{}\\i{} ",
            i32::from(bold),
            i32::from(italic),
        ));
        let s = String::from_utf8_lossy(&run.bytes);
        for c in s.chars() {
            rtf_escape_char_into(&mut rtf, c);
        }
    }

    rtf.push_str("\n}\n");
    rtf
}

/// Build the per-style attribute table for every style index that
/// actually appears in `runs`. Each unique style index is queried
/// exactly once.
fn collect_style_attrs(sci: Hwnd, runs: &[StyleRun]) -> Vec<(u8, StyleAttrs)> {
    let mut seen = std::collections::BTreeSet::new();
    seen.insert(STYLE_DEFAULT as u8);
    for r in runs {
        seen.insert(r.style);
    }
    seen.into_iter()
        .map(|s| (s, query_style_attrs(sci, s)))
        .collect()
}

/// Build the HTML for the active buffer. Common path for both
/// "Export to HTML..." and "Copy HTML to Clipboard"; returns the
/// full document as a `String`.
fn export_html_for_active() -> String {
    let sci = sdk::active_scintilla();
    if sci.is_null() {
        return String::new();
    }
    // SAFETY: SCI_GETLENGTH takes no pointer; pure query.
    let len = unsafe { sdk::SendMessageW(sci, SCI_GETLENGTH, 0, 0) };
    let len_us = match usize::try_from(len) {
        Ok(n) => n,
        Err(_) => return String::new(),
    };
    if len_us == 0 {
        return String::new();
    }
    let (text, styles) = collect_text_and_styles(sci, len_us);
    if text.is_empty() {
        return String::new();
    }
    let runs = group_runs(&text, &styles);
    let attrs = collect_style_attrs(sci, &runs);
    build_html(&runs, &attrs)
}

// ---- Win32 helpers (clipboard, file save dialog) ----

// ---- Menu callbacks ----
//
// The two host services — `sdk::export_save_dialog` (native Save-As +
// file write) and `sdk::set_clipboard` (system clipboard) — do all the
// OS-specific work, so these callbacks are pure "render bytes, hand
// them off" and identical on every platform.

extern "C" fn cmd_export_html() {
    let html = export_html_for_active();
    if html.is_empty() {
        sdk::set_status("Export: empty buffer");
        return;
    }
    // The host runs the dialog, writes the file, and reports the outcome
    // on the status bar — including the silent-on-cancel case.
    sdk::export_save_dialog(html.as_bytes(), "export.html", EXPORT_KIND_HTML);
}

extern "C" fn cmd_copy_html() {
    let html = export_html_for_active();
    if html.is_empty() {
        sdk::set_status("Export: empty buffer");
        return;
    }
    if sdk::set_clipboard(&[(CLIP_FORMAT_HTML, html.as_bytes())]) {
        sdk::set_status("Export: HTML copied to clipboard");
    } else {
        sdk::set_status("Export: clipboard write failed");
    }
}

extern "C" fn cmd_export_rtf() {
    let rtf = export_rtf_for_active();
    if rtf.is_empty() {
        sdk::set_status("Export: empty buffer");
        return;
    }
    sdk::export_save_dialog(rtf.as_bytes(), "export.rtf", EXPORT_KIND_RTF);
}

extern "C" fn cmd_copy_rtf() {
    let rtf = export_rtf_for_active();
    if rtf.is_empty() {
        sdk::set_status("Export: empty buffer");
        return;
    }
    if sdk::set_clipboard(&[(CLIP_FORMAT_RTF, rtf.as_bytes())]) {
        sdk::set_status("Export: RTF copied to clipboard");
    } else {
        sdk::set_status("Export: clipboard write failed");
    }
}

/// Render the active buffer in **all three** clipboard formats — plain,
/// HTML, and RTF — in one call so the receiving app picks whichever it
/// understands best. The host packages each abstract format for the
/// platform (Win32 wraps HTML in `CF_HTML`, etc.). Notepad++'s upstream
/// `NppExport` ships the same item under the same label.
extern "C" fn cmd_copy_all() {
    let sci = sdk::active_scintilla();
    if sci.is_null() {
        return;
    }
    // SAFETY: SCI_GETLENGTH takes no pointer; pure query.
    let len = unsafe { sdk::SendMessageW(sci, SCI_GETLENGTH, 0, 0) };
    let len_us = match usize::try_from(len) {
        Ok(n) => n,
        Err(_) => return,
    };
    if len_us == 0 {
        sdk::set_status("Export: empty buffer");
        return;
    }
    let (text, styles) = collect_text_and_styles(sci, len_us);
    if text.is_empty() {
        sdk::set_status("Export: empty buffer");
        return;
    }
    let runs = group_runs(&text, &styles);
    let attrs = collect_style_attrs(sci, &runs);
    let html = build_html(&runs, &attrs);
    let rtf = build_rtf(&runs, &attrs);
    // Plain text is the buffer's bytes decoded as UTF-8 (lossy on
    // invalid sequences) — the lowest-common-denominator paste target.
    let plain = String::from_utf8_lossy(&text).into_owned();

    let ok = sdk::set_clipboard(&[
        (CLIP_FORMAT_PLAIN, plain.as_bytes()),
        (CLIP_FORMAT_HTML, html.as_bytes()),
        (CLIP_FORMAT_RTF, rtf.as_bytes()),
    ]);
    if ok {
        sdk::set_status("Export: HTML + RTF + plain copied to clipboard");
    } else {
        sdk::set_status("Export: clipboard write failed");
    }
}

/// Build the RTF for the active buffer. Common path for both
/// "Export to RTF..." and "Copy RTF to Clipboard"; mirrors the
/// shape of `export_html_for_active` so the two formats stay in
/// lockstep.
fn export_rtf_for_active() -> String {
    let sci = sdk::active_scintilla();
    if sci.is_null() {
        return String::new();
    }
    // SAFETY: SCI_GETLENGTH takes no pointer; pure query.
    let len = unsafe { sdk::SendMessageW(sci, SCI_GETLENGTH, 0, 0) };
    let len_us = match usize::try_from(len) {
        Ok(n) => n,
        Err(_) => return String::new(),
    };
    if len_us == 0 {
        return String::new();
    }
    let (text, styles) = collect_text_and_styles(sci, len_us);
    if text.is_empty() {
        return String::new();
    }
    let runs = group_runs(&text, &styles);
    let attrs = collect_style_attrs(sci, &runs);
    build_rtf(&runs, &attrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(fore: u32, back: u32, bold: bool, italic: bool) -> StyleAttrs {
        StyleAttrs {
            fore_rgb: fore,
            back_rgb: back,
            bold,
            italic,
            size_pts: 11,
            font_name: String::from("Consolas"),
        }
    }

    #[test]
    fn sanitize_font_name_strips_style_injection() {
        // The injection vector: a font name containing `</style>` would
        // close the <style> block in the HTML output and let arbitrary
        // markup through. The sanitizer drops every `<`, `>`, `/`, `;`
        // and other CSS-meaningful characters.
        let evil = "</style><script>alert(1)</script><style";
        let cleaned = sanitize_font_name(evil);
        assert!(!cleaned.contains('<'));
        assert!(!cleaned.contains('>'));
        assert!(!cleaned.contains('/'));
        // The text payload (`stylescriptalert1scriptstyle`) survives
        // as a single safe identifier — harmless in CSS.
    }

    #[test]
    fn sanitize_font_name_preserves_real_names() {
        // Real font names should pass through. Spaces, hyphens, and
        // underscores are allowed in the allowlist.
        assert_eq!(sanitize_font_name("Consolas"), "Consolas");
        assert_eq!(sanitize_font_name("Courier New"), "Courier New");
        assert_eq!(sanitize_font_name("DejaVu Sans Mono"), "DejaVu Sans Mono");
        assert_eq!(sanitize_font_name("JetBrains_Mono-NL"), "JetBrains_Mono-NL");
    }

    #[test]
    fn build_html_does_not_emit_close_style_in_body() {
        // End-to-end injection check: a malicious font name on the
        // default style must NOT result in a literal `</style>` in
        // the output BEFORE the legitimate one that closes the
        // body's CSS block.
        let runs = vec![StyleRun {
            style: STYLE_DEFAULT as u8,
            bytes: b"x".to_vec(),
        }];
        let mut a = attrs(0, 0x00_FF_FF_FF, false, false);
        a.font_name = String::from("</style><script>alert(1)</script>");
        let attrs_table = vec![(STYLE_DEFAULT as u8, a)];
        let html = build_html(&runs, &attrs_table);
        // There should be exactly one `</style>` (the closing tag),
        // not two (the legitimate close + an injected one before it).
        assert_eq!(html.matches("</style>").count(), 1);
        // Belt and suspenders: the injection text must not appear.
        assert!(!html.contains("<script>"));
        assert!(!html.contains("alert(1)"));
    }

    #[test]
    fn rgb_to_css_swaps_byte_order() {
        // Scintilla's `0x00BBGGRR` packs R in the low byte; CSS
        // expects "#RRGGBB". Pinning so a future RGB tweak doesn't
        // silently flip the colors blue<->red.
        assert_eq!(rgb_to_css(0x00_00_00_FF), "#FF0000"); // red
        assert_eq!(rgb_to_css(0x00_00_FF_00), "#00FF00"); // green
        assert_eq!(rgb_to_css(0x00_FF_00_00), "#0000FF"); // blue
        assert_eq!(rgb_to_css(0x00_FF_FF_FF), "#FFFFFF");
        assert_eq!(rgb_to_css(0x00_00_00_00), "#000000");
    }

    #[test]
    fn group_runs_empty() {
        assert_eq!(group_runs(b"", &[]), vec![]);
    }

    #[test]
    fn group_runs_single_style() {
        let text = b"hello";
        let styles = vec![5u8; text.len()];
        assert_eq!(
            group_runs(text, &styles),
            vec![StyleRun {
                style: 5,
                bytes: text.to_vec(),
            }],
        );
    }

    #[test]
    fn group_runs_collapses_adjacent() {
        let text = b"abcXYZdef";
        // "abc" → style 1, "XYZ" → style 2, "def" → style 1.
        let styles = vec![1, 1, 1, 2, 2, 2, 1, 1, 1];
        assert_eq!(
            group_runs(text, &styles),
            vec![
                StyleRun {
                    style: 1,
                    bytes: b"abc".to_vec()
                },
                StyleRun {
                    style: 2,
                    bytes: b"XYZ".to_vec()
                },
                StyleRun {
                    style: 1,
                    bytes: b"def".to_vec()
                },
            ],
        );
    }

    #[test]
    fn group_runs_alternating_per_byte() {
        // Worst case for run collapse: every byte differs from its
        // neighbour. Should produce one run per byte.
        let text = b"abcd";
        let styles = vec![1, 2, 3, 4];
        let runs = group_runs(text, &styles);
        assert_eq!(runs.len(), 4);
        for (i, run) in runs.iter().enumerate() {
            assert_eq!(run.style, (i + 1) as u8);
            assert_eq!(run.bytes.len(), 1);
            assert_eq!(run.bytes[0], text[i]);
        }
    }

    #[test]
    fn group_runs_single_byte() {
        // Edge case: the loop `for i in 1..n` never executes when
        // n == 1, so the final `runs.push` alone handles the output.
        // Pinning so a refactor that fuses the push into the loop
        // doesn't accidentally drop the single-byte case.
        let runs = group_runs(b"a", &[7]);
        assert_eq!(
            runs,
            vec![StyleRun {
                style: 7,
                bytes: b"a".to_vec(),
            }],
        );
    }

    #[test]
    fn group_runs_truncates_to_min_length() {
        // Defensive: if the styles slice is shorter than the text,
        // we don't index past it. Production paths keep them aligned
        // (one style per byte), but this prevents a panic if a
        // future code path drifts.
        let text = b"abcdef";
        let styles = vec![1, 1, 2];
        let runs = group_runs(text, &styles);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].bytes, b"ab");
        assert_eq!(runs[1].bytes, b"c");
    }

    #[test]
    fn escape_html_basic_specials() {
        let mut s = String::new();
        escape_html_into(&mut s, b"a & b < c > d \" e");
        assert_eq!(s, "a &amp; b &lt; c &gt; d &quot; e");
    }

    #[test]
    fn escape_html_passes_through_unicode() {
        let mut s = String::new();
        // "é" is 0xC3 0xA9 in UTF-8 — should round-trip through the
        // utf8-lossy decode and out as the literal char.
        escape_html_into(&mut s, &[0xC3, 0xA9]);
        assert_eq!(s, "é");
    }

    #[test]
    fn escape_html_lossy_on_invalid_utf8() {
        let mut s = String::new();
        // Lone 0xFF byte is invalid UTF-8; from_utf8_lossy substitutes
        // the replacement character. Pin the substitution explicitly
        // so a future swap to a non-lossy decoder becomes a deliberate
        // decision (the panic-on-invalid alternative would crash the
        // host on a binary file the user opened).
        escape_html_into(&mut s, &[0xFF]);
        assert!(s.contains('\u{FFFD}'), "lossy substitution missing: {s:?}");
    }

    #[test]
    fn build_html_emits_doctype_meta_title() {
        let runs = vec![StyleRun {
            style: STYLE_DEFAULT as u8,
            bytes: b"hello".to_vec(),
        }];
        let attrs = vec![(
            STYLE_DEFAULT as u8,
            attrs(0x00_00_00_00, 0x00_FF_FF_FF, false, false),
        )];
        let html = build_html(&runs, &attrs);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<meta charset=\"UTF-8\">"));
        assert!(html.contains("<title>Exported from Code++</title>"));
        assert!(html.contains("white-space: pre"));
        assert!(html.contains("hello"));
        assert!(html.ends_with("</body>\n</html>\n"));
    }

    #[test]
    fn build_html_emits_per_style_class() {
        let runs = vec![
            StyleRun {
                style: STYLE_DEFAULT as u8,
                bytes: b"plain ".to_vec(),
            },
            StyleRun {
                style: 5,
                bytes: b"red".to_vec(),
            },
        ];
        let attrs = vec![
            (
                STYLE_DEFAULT as u8,
                attrs(0x00_00_00_00, 0x00_FF_FF_FF, false, false),
            ),
            (5, attrs(0x00_00_00_FF, 0x00_FF_FF_FF, true, false)),
        ];
        let html = build_html(&runs, &attrs);
        // Style 5 gets a CSS class with the red color and bold.
        assert!(html.contains(".s5 { color: #FF0000; font-weight: bold; }"));
        // The default-style run is wrapped in a plain <span>, no class.
        assert!(html.contains("<span>plain </span>"));
        // The red run uses the class.
        assert!(html.contains("<span class=\"s5\">red</span>"));
    }

    #[test]
    fn build_html_omits_background_when_same_as_default() {
        let runs = vec![StyleRun {
            style: 5,
            bytes: b"x".to_vec(),
        }];
        let attrs = vec![
            (STYLE_DEFAULT as u8, attrs(0, 0x00_FF_FF_FF, false, false)),
            // Same background as default — should not be repeated.
            (5, attrs(0x00_00_00_FF, 0x00_FF_FF_FF, false, false)),
        ];
        let html = build_html(&runs, &attrs);
        assert!(html.contains(".s5 { color: #FF0000; }"));
        assert!(!html.contains(".s5 { color: #FF0000; background-color"));
    }

    #[test]
    fn build_html_includes_background_when_distinct() {
        let runs = vec![StyleRun {
            style: 6,
            bytes: b"y".to_vec(),
        }];
        let attrs = vec![
            (STYLE_DEFAULT as u8, attrs(0, 0x00_FF_FF_FF, false, false)),
            (6, attrs(0x00_00_00_00, 0x00_AA_BB_CC, false, false)),
        ];
        let html = build_html(&runs, &attrs);
        assert!(html.contains("background-color: #CCBBAA"));
    }

    #[test]
    fn build_html_escapes_special_characters() {
        let runs = vec![StyleRun {
            style: STYLE_DEFAULT as u8,
            bytes: b"if (x < 10 && y > 0)".to_vec(),
        }];
        let attrs = vec![(STYLE_DEFAULT as u8, attrs(0, 0x00_FF_FF_FF, false, false))];
        let html = build_html(&runs, &attrs);
        assert!(html.contains("if (x &lt; 10 &amp;&amp; y &gt; 0)"));
        assert!(!html.contains("if (x < 10"));
    }

    #[test]
    fn build_html_preserves_newlines_via_white_space_pre() {
        // Newlines in source code show up as literal `\n` bytes in
        // the run text. The body's `white-space: pre` rule renders
        // them as line breaks; we must not escape or transform them
        // in `escape_html_into`. Pin both sides: the CSS rule is
        // present and the literal newline survives in the output.
        let runs = vec![StyleRun {
            style: STYLE_DEFAULT as u8,
            bytes: b"line1\nline2".to_vec(),
        }];
        let attrs = vec![(STYLE_DEFAULT as u8, attrs(0, 0x00_FF_FF_FF, false, false))];
        let html = build_html(&runs, &attrs);
        assert!(html.contains("white-space: pre"));
        assert!(html.contains("line1\nline2"));
    }

    #[test]
    fn build_html_falls_back_when_default_style_absent() {
        // No STYLE_DEFAULT entry in the attrs table — the function
        // should pick reasonable defaults rather than panic.
        let runs = vec![StyleRun {
            style: 1,
            bytes: b"x".to_vec(),
        }];
        let attrs = vec![(1u8, attrs(0x00_00_00_FF, 0x00_FF_FF_FF, false, false))];
        let html = build_html(&runs, &attrs);
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("Consolas") || html.contains("monospace"));
    }

    // ---- RTF tests ----

    fn rtf_escape(s: &str) -> String {
        let mut out = String::new();
        for c in s.chars() {
            rtf_escape_char_into(&mut out, c);
        }
        out
    }

    #[test]
    fn rtf_escape_passes_printable_ascii() {
        assert_eq!(rtf_escape("Hello, world!"), "Hello, world!");
    }

    #[test]
    fn rtf_escape_braces_and_backslash() {
        // The three RTF control characters get prefixed with `\`.
        assert_eq!(rtf_escape("a\\b{c}d"), "a\\\\b\\{c\\}d");
    }

    #[test]
    fn rtf_escape_newline_becomes_par() {
        assert_eq!(rtf_escape("a\nb"), "a\\par\nb");
    }

    #[test]
    fn rtf_escape_crlf_collapses_to_par() {
        // `\r` is dropped, `\n` becomes `\par\n` — net effect for
        // a CRLF input is a single paragraph break, matching the
        // single-paragraph semantic of CRLF.
        assert_eq!(rtf_escape("a\r\nb"), "a\\par\nb");
    }

    #[test]
    fn rtf_escape_tab_emits_control_word() {
        // `\tab ` with a trailing space — RTF parses control words
        // by reading until a non-letter, so the space delimits the
        // word from following text.
        assert_eq!(rtf_escape("a\tb"), "a\\tab b");
    }

    #[test]
    fn rtf_escape_non_ascii_emits_unicode_escape() {
        // `é` is U+00E9 = 233. Not a surrogate, so one `\u233?`.
        assert_eq!(rtf_escape("é"), "\\u233?");
    }

    #[test]
    fn rtf_escape_non_bmp_emits_surrogate_pair() {
        // `🎉` is U+1F389 = 127881. UTF-16 encodes that as a
        // surrogate pair (0xD83C, 0xDF89). Both halves emit as
        // signed-16 values: 0xD83C = -10180, 0xDF89 = -8311.
        assert_eq!(rtf_escape("🎉"), "\\u-10180?\\u-8311?");
    }

    #[test]
    fn rtf_escape_high_byte_in_range() {
        // 0x7F is DEL — not in the printable range (0x20..=0x7E),
        // so it goes through the unicode path. As u16 = 127.
        assert_eq!(rtf_escape("\x7f"), "\\u127?");
    }

    #[test]
    fn build_rtf_emits_preamble_fonttbl_colortbl() {
        let runs = vec![StyleRun {
            style: STYLE_DEFAULT as u8,
            bytes: b"hello".to_vec(),
        }];
        let attrs = vec![(
            STYLE_DEFAULT as u8,
            attrs(0x00_00_00_00, 0x00_FF_FF_FF, false, false),
        )];
        let rtf = build_rtf(&runs, &attrs);
        assert!(rtf.starts_with("{\\rtf1\\ansi"));
        assert!(rtf.contains("\\fonttbl"));
        assert!(rtf.contains("\\colortbl;"));
        assert!(rtf.contains("\\fmodern Consolas;"));
        assert!(rtf.ends_with("}\n"));
        assert!(rtf.contains("hello"));
    }

    #[test]
    fn build_rtf_emits_per_run_state() {
        let runs = vec![
            StyleRun {
                style: STYLE_DEFAULT as u8,
                bytes: b"plain ".to_vec(),
            },
            StyleRun {
                style: 5,
                bytes: b"red".to_vec(),
            },
        ];
        let attrs = vec![
            (
                STYLE_DEFAULT as u8,
                attrs(0x00_00_00_00, 0x00_FF_FF_FF, false, false),
            ),
            (5, attrs(0x00_00_00_FF, 0x00_FF_FF_FF, true, false)),
        ];
        let rtf = build_rtf(&runs, &attrs);
        // Every run gets explicit \cf, \b, \i markers — no
        // inheritance ambiguity at run boundaries.
        assert!(rtf.contains("\\cf"));
        assert!(rtf.contains("\\b1"));
        assert!(rtf.contains("\\b0"));
        assert!(rtf.contains("\\i0"));
    }

    #[test]
    fn build_rtf_dedupes_identical_colors() {
        // Two styles with the same fore_rgb should share a color
        // table index — the table has one user entry, not two.
        let runs = vec![
            StyleRun {
                style: 1,
                bytes: b"a".to_vec(),
            },
            StyleRun {
                style: 2,
                bytes: b"b".to_vec(),
            },
        ];
        let attrs = vec![
            (1u8, attrs(0x00_00_00_FF, 0x00_FF_FF_FF, false, false)),
            (2u8, attrs(0x00_00_00_FF, 0x00_FF_FF_FF, true, false)),
        ];
        let rtf = build_rtf(&runs, &attrs);
        // Exactly one `\redR\greenG\blueB;` in the colour table.
        let red_count = rtf.matches("\\red").count();
        assert_eq!(
            red_count, 1,
            "duplicate fore_rgb values should share a colour-table index: {rtf:?}",
        );
    }

    #[test]
    fn build_rtf_emits_colors_in_correct_byte_order() {
        // Scintilla packs RGB as 0x00BBGGRR; our colour-table
        // emission pulls the bytes apart and writes them in the
        // RTF \red\green\blue order. Pin the ordering so a future
        // refactor that flips R and B is caught.
        let runs = vec![StyleRun {
            style: 1,
            bytes: b"x".to_vec(),
        }];
        // 0x00_00_00_FF = pure red in Scintilla's packing.
        let attrs = vec![(1u8, attrs(0x00_00_00_FF, 0x00_FF_FF_FF, false, false))];
        let rtf = build_rtf(&runs, &attrs);
        assert!(
            rtf.contains("\\red255\\green0\\blue0"),
            "expected red entry in colour table: {rtf:?}",
        );
    }

    #[test]
    fn build_rtf_escapes_braces_in_run_text() {
        // The body's per-run text goes through rtf_escape_char_into,
        // so `{` and `}` and `\` survive as escape sequences rather
        // than corrupting the RTF structure.
        let runs = vec![StyleRun {
            style: STYLE_DEFAULT as u8,
            bytes: b"if (x) { y \\ z; }".to_vec(),
        }];
        let attrs = vec![(STYLE_DEFAULT as u8, attrs(0, 0x00_FF_FF_FF, false, false))];
        let rtf = build_rtf(&runs, &attrs);
        assert!(rtf.contains("if (x) \\{ y \\\\ z; \\}"));
    }

    #[test]
    fn build_rtf_sanitizes_font_name() {
        // Font name made entirely of RTF control characters
        // collapses to empty after sanitization → falls back to
        // "Consolas". Pin both the fallback presence AND the
        // absence of the injected control words so a future
        // refactor can't silently emit a malformed `\fmodern ;`
        // group OR let a control character reach the wire.
        let runs = vec![StyleRun {
            style: STYLE_DEFAULT as u8,
            bytes: b"x".to_vec(),
        }];
        let mut a = attrs(0, 0x00_FF_FF_FF, false, false);
        a.font_name = String::from("}{\\");
        let attrs_table = vec![(STYLE_DEFAULT as u8, a)];
        let rtf = build_rtf(&runs, &attrs_table);
        assert!(
            rtf.contains("{\\fonttbl{\\f0\\fmodern Consolas;}}"),
            "empty sanitized font name should fall back to Consolas: {rtf:?}",
        );
    }

    // The CF_HTML byte-offset wrapper moved to the Win32 backend
    // (`ui_win32::build_cf_html`) when clipboard packaging became a host
    // responsibility; its tests live there now.

    #[test]
    fn build_rtf_font_name_partial_sanitization() {
        // Mixed name "Bad}\\b" sanitizes to "Badb" (the `}` and
        // `\` are stripped, but the `b` survives the allowlist).
        // Pin so a future allowlist tightening doesn't silently
        // accept what should now be filtered.
        let runs = vec![StyleRun {
            style: STYLE_DEFAULT as u8,
            bytes: b"x".to_vec(),
        }];
        let mut a = attrs(0, 0x00_FF_FF_FF, false, false);
        a.font_name = String::from("Bad}\\b");
        let attrs_table = vec![(STYLE_DEFAULT as u8, a)];
        let rtf = build_rtf(&runs, &attrs_table);
        assert!(
            rtf.contains("{\\fonttbl{\\f0\\fmodern Badb;}}"),
            "expected sanitized 'Badb' in font table: {rtf:?}",
        );
        // No literal `}` between `\fmodern ` and the `;` — that
        // would close the fonttbl group early.
        assert!(
            !rtf.contains("\\fmodern Bad}"),
            "stray `}}` should not survive sanitization: {rtf:?}",
        );
    }
}
