//! Entry-point implementations for cppexport.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! Four menu items, two HTML and two RTF, each in an
//! Export-to-file plus Copy-to-clipboard pair:
//!   * **Export to HTML...** — `GetSaveFileNameW` for the
//!     destination path, then write the HTML there.
//!   * **Copy HTML to Clipboard** — same HTML, but to
//!     `CF_UNICODETEXT` on the system clipboard so the user can
//!     paste it into a browser, an email, or a `.html` file
//!     directly.
//!   * **Export to RTF...** — same as Export to HTML but emits
//!     RTF (Rich Text Format) for paste into Word, Outlook,
//!     WordPad, or LibreOffice.
//!   * **Copy RTF to Clipboard** — same RTF, but to the
//!     registered `Rich Text Format` clipboard type that
//!     RTF-aware editors prefer.
//!
//! Style extraction is straightforward but per-character: walk the
//! buffer with `SCI_GETSTYLEAT(pos)`, collapse adjacent same-style
//! runs, then query `SCI_STYLEGET{FORE,BACK,BOLD,ITALIC,SIZE,FONT}`
//! for each unique style. The output uses class-based CSS so a
//! 5000-line file with 8 active styles emits 8 CSS rules and one
//! `<span>` per run, not one per character.

#![cfg(target_os = "windows")]

use core::ffi::{c_char, c_void};

use codepp_plugin_sdk::{self as sdk, FuncItem, Hwnd, NppData, SCNotification, SyncCell};

// Clipboard fns live in user32 alongside the SDK's `SendMessageW`.
// Listing them in a separate extern block (with the same
// `#[link]`) compiles cleanly — the linker dedupes the duplicate
// `user32` entry — and keeps the clipboard surface visible
// here rather than buried in the SDK's general-purpose header.
#[link(name = "user32")]
extern "system" {
    fn OpenClipboard(hwnd: Hwnd) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, hmem: *mut c_void) -> *mut c_void;
    fn RegisterClipboardFormatA(format_name: *const c_char) -> u32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(hmem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(hmem: *mut c_void) -> i32;
    fn GlobalFree(hmem: *mut c_void) -> *mut c_void;
}

#[link(name = "comdlg32")]
extern "system" {
    fn GetSaveFileNameW(ofn: *mut OpenFileName) -> i32;
}

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

/// Mirror of Scintilla's `Sci_CharacterRangeFull` (Sci_Position
/// = ptrdiff_t, so two pointer-sized signed integers).
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

// Win32 clipboard / global-memory constants.
const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

// Win32 OPENFILENAMEW flags.
const OFN_OVERWRITEPROMPT: u32 = 0x0000_0002;
const OFN_HIDEREADONLY: u32 = 0x0000_0004;
const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
const OFN_EXPLORER: u32 = 0x0008_0000;
const MAX_PATH: usize = 260;

#[repr(C)]
struct OpenFileName {
    l_struct_size: u32,
    hwnd_owner: Hwnd,
    h_instance: Hwnd,
    lp_str_filter: *const u16,
    lp_str_custom_filter: *mut u16,
    n_max_cust_filter: u32,
    n_filter_index: u32,
    lp_str_file: *mut u16,
    n_max_file: u32,
    lp_str_file_title: *mut u16,
    n_max_file_title: u32,
    lp_str_initial_dir: *const u16,
    lp_str_title: *const u16,
    flags: u32,
    n_file_offset: u16,
    n_file_extension: u16,
    lp_str_def_ext: *const u16,
    l_cust_data: isize,
    lpfn_hook: *mut c_void,
    lp_template_name: *const u16,
    pv_reserved: *mut c_void,
    dw_reserved: u32,
    flags_ex: u32,
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
/// source file that's 200,000 SendMessage calls; cppexport users
/// reported export-button latency. `SCI_GETSTYLEDTEXTFULL` returns
/// the same data in one call by writing alternating
/// (text_byte, style_byte) pairs into a caller-supplied buffer.
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
        sdk::SendMessageW(
            sci,
            SCI_GETSTYLEDTEXTFULL,
            0,
            &mut range as *mut SciTextRangeFull as isize,
        );
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
/// ASCII (or UTF-8) font name. Two-phase like SCI_GETSELTEXT: pass
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
        .find(|(s, _)| *s as u32 == STYLE_DEFAULT)
        .map(|(_, a)| a.clone())
        .unwrap_or(StyleAttrs {
            fore_rgb: 0x00_00_00_00,
            back_rgb: 0x00_FF_FF_FF,
            bold: false,
            italic: false,
            size_pts: 11,
            font_name: String::from("Consolas"),
        });

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
        if *style as u32 == STYLE_DEFAULT {
            continue;
        }
        html.push_str(&format!(".s{} {{ ", style));
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
        if run.style as u32 == STYLE_DEFAULT {
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
        .find(|(s, _)| *s as u32 == STYLE_DEFAULT)
        .map(|(_, a)| a.clone())
        .unwrap_or(StyleAttrs {
            fore_rgb: 0x00_00_00_00,
            back_rgb: 0x00_FF_FF_FF,
            bold: false,
            italic: false,
            size_pts: 11,
            font_name: String::from("Consolas"),
        });

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
        let bold = attrs.map(|a| a.bold).unwrap_or(false);
        let italic = attrs.map(|a| a.italic).unwrap_or(false);
        rtf.push_str(&format!(
            "\\cf{cf_idx}\\b{}\\i{} ",
            if bold { 1 } else { 0 },
            if italic { 1 } else { 0 },
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

/// Copy `s` (a UTF-8 string) onto the Windows clipboard as
/// `CF_UNICODETEXT`. Returns `true` on success. The clipboard takes
/// ownership of the global memory we allocate; per the API, we must
/// not free it ourselves once `SetClipboardData` succeeds.
fn copy_to_clipboard(s: &str) -> bool {
    let wide: Vec<u16> = s.encode_utf16().chain(core::iter::once(0)).collect();
    // SAFETY: cast `&[u16]` to `&[u8]` of the same byte length so
    // `global_alloc_copy` sees the raw bytes. `wide` outlives the
    // call. The byte-length multiply is bounded — `wide.len()` is
    // already at most `isize::MAX / 2`, so doubling can't wrap.
    let bytes = match wide.len().checked_mul(core::mem::size_of::<u16>()) {
        Some(n) => n,
        None => return false,
    };
    let wide_bytes = unsafe { core::slice::from_raw_parts(wide.as_ptr() as *const u8, bytes) };
    set_clipboard_format(CF_UNICODETEXT, wide_bytes, false)
}

/// Show the standard Windows "Save As" dialog and return the chosen
/// path on success, or `None` if the user cancelled. Caller-supplied
/// filter pieces let HTML and RTF callers share the dialog plumbing
/// without duplicating the OPENFILENAMEW setup.
///
/// `filter_label` is what the user sees in the "Files of type"
/// combo (e.g. `"HTML Files (*.html)"`); `filter_glob` is the match
/// pattern (e.g. `"*.html"`); `default_ext` is the extension Win32
/// appends when the user types a name without one (e.g. `"html"`).
fn prompt_save_path(filter_label: &str, filter_glob: &str, default_ext: &str) -> Option<String> {
    let npp = sdk::npp_handle();

    // Filter string: pairs of NUL-terminated wide strings, terminated
    // by a double-NUL — Win32 parses them as (display, glob) pairs
    // and stops at the empty pair. We always include an "All Files"
    // fallback so the user can pick anything.
    let filter_str = format!("{filter_label}\0{filter_glob}\0All Files (*.*)\0*.*\0\0");
    let filter: Vec<u16> = filter_str.encode_utf16().collect();

    let default_ext_str = format!("{default_ext}\0");
    let default_ext: Vec<u16> = default_ext_str.encode_utf16().collect();

    // Path buffer: must be at least MAX_PATH wide chars and zeroed.
    // GetSaveFileNameW writes the chosen path into it (including the
    // appended default extension on success).
    let mut path = vec![0u16; MAX_PATH + 1];

    let mut ofn = OpenFileName {
        l_struct_size: core::mem::size_of::<OpenFileName>() as u32,
        hwnd_owner: npp,
        h_instance: core::ptr::null_mut(),
        lp_str_filter: filter.as_ptr(),
        lp_str_custom_filter: core::ptr::null_mut(),
        n_max_cust_filter: 0,
        n_filter_index: 1,
        lp_str_file: path.as_mut_ptr(),
        n_max_file: path.len() as u32,
        lp_str_file_title: core::ptr::null_mut(),
        n_max_file_title: 0,
        lp_str_initial_dir: core::ptr::null(),
        lp_str_title: core::ptr::null(),
        flags: OFN_OVERWRITEPROMPT | OFN_HIDEREADONLY | OFN_PATHMUSTEXIST | OFN_EXPLORER,
        n_file_offset: 0,
        n_file_extension: 0,
        lp_str_def_ext: default_ext.as_ptr(),
        l_cust_data: 0,
        lpfn_hook: core::ptr::null_mut(),
        lp_template_name: core::ptr::null(),
        pv_reserved: core::ptr::null_mut(),
        dw_reserved: 0,
        flags_ex: 0,
    };

    // SAFETY: `&mut ofn` is a valid pointer to an `OpenFileName`
    // struct fully initialized above. All buffers it references are
    // owned in this scope and outlive the call. Returns 0 on
    // user-cancel or error; non-zero on success.
    let ok = unsafe { GetSaveFileNameW(&mut ofn) };
    if ok == 0 {
        return None;
    }

    // Find the NUL terminator and decode.
    let nul = path.iter().position(|&u| u == 0).unwrap_or(path.len());
    Some(String::from_utf16_lossy(&path[..nul]))
}

// ---- Menu callbacks ----

extern "C" fn cmd_export_html() {
    let html = export_html_for_active();
    if html.is_empty() {
        sdk::set_status("Export: empty buffer");
        return;
    }
    let Some(path) = prompt_save_path("HTML Files (*.html)", "*.html", "html") else {
        // User cancelled; no status update so the previous status
        // line stays visible (matches N++'s "silent cancel" UX).
        return;
    };
    match std::fs::write(&path, &html) {
        Ok(()) => sdk::set_status(&format!("Export: wrote {path}")),
        Err(e) => sdk::set_status(&format!("Export failed: {e}")),
    }
}

extern "C" fn cmd_copy_html() {
    let html = export_html_for_active();
    if html.is_empty() {
        sdk::set_status("Export: empty buffer");
        return;
    }
    if copy_to_clipboard(&html) {
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
    let Some(path) = prompt_save_path("RTF Files (*.rtf)", "*.rtf", "rtf") else {
        return;
    };
    match std::fs::write(&path, &rtf) {
        Ok(()) => sdk::set_status(&format!("Export: wrote {path}")),
        Err(e) => sdk::set_status(&format!("Export failed: {e}")),
    }
}

extern "C" fn cmd_copy_rtf() {
    let rtf = export_rtf_for_active();
    if rtf.is_empty() {
        sdk::set_status("Export: empty buffer");
        return;
    }
    if copy_rtf_to_clipboard(&rtf) {
        sdk::set_status("Export: RTF copied to clipboard");
    } else {
        sdk::set_status("Export: clipboard write failed");
    }
}

/// Render the active buffer in **all three** clipboard formats —
/// CF_UNICODETEXT (plain), CF_HTML (styled HTML wrapped per the
/// CF_HTML byte-offset spec), and the registered "Rich Text
/// Format" — so the receiving app can pick whichever it understands
/// best. Notepad++'s upstream `NppExport` ships the same item
/// under the same label.
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
    // invalid sequences). Goes onto CF_UNICODETEXT as the
    // lowest-common-denominator paste target.
    let plain = String::from_utf8_lossy(&text).into_owned();

    if copy_all_formats_to_clipboard(&plain, &html, &rtf) {
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

/// Copy `rtf` (a plain ASCII RTF document — non-ASCII codepoints
/// are encoded as `\uN?` escapes inside the RTF body) onto the
/// clipboard under the registered "Rich Text Format" type. Word,
/// Outlook, WordPad, and other RTF-aware editors paste from this
/// format with formatting preserved.
fn copy_rtf_to_clipboard(rtf: &str) -> bool {
    let cf_rtf = register_rtf_format();
    if cf_rtf == 0 {
        return false;
    }
    set_clipboard_format(cf_rtf, rtf.as_bytes(), true)
}

/// Copy three formats — plain text (CF_UNICODETEXT), CF_HTML, and
/// "Rich Text Format" — onto the clipboard in a single
/// Open/Empty/Close sequence. The receiving app picks whichever
/// format it understands (Word/Outlook prefer CF_HTML or RTF;
/// browser-based editors that paste-as-HTML get the CF_HTML; plain
/// editors fall back to the unicode text). Mirrors NppExport's
/// "Copy all formats" flow.
///
/// Returns `true` if at least one of the three formats was
/// accepted by the clipboard. The Win32 contract: `SetClipboardData`
/// returns the handle (= success, clipboard now owns the memory)
/// or null (= failure, plugin must `GlobalFree`). We track each
/// format independently and free per-format on failure.
fn copy_all_formats_to_clipboard(plain: &str, html: &str, rtf: &str) -> bool {
    // Format registrations are independent: a failure to register
    // CF_HTML or "Rich Text Format" (~impossible — only happens
    // when the system-wide registered-format table is exhausted)
    // means *that one* format gets skipped; the others still ship.
    // CF_UNICODETEXT is built-in (id 13) so it never needs
    // registration and never gets skipped.
    let cf_html = register_html_format();
    let cf_rtf = register_rtf_format();

    // `chain(once(0))` bakes the NUL terminator that CF_UNICODETEXT
    // requires into the wide-char vector before we hand it to
    // `global_alloc_copy(.., false)` — so the `nul_terminate=false`
    // arg below is correct, not a mistake.
    let plain_wide: Vec<u16> = plain.encode_utf16().chain(core::iter::once(0)).collect();
    let plain_byte_len = match plain_wide.len().checked_mul(core::mem::size_of::<u16>()) {
        Some(n) => n,
        None => return false,
    };

    let npp = sdk::npp_handle();

    // SAFETY: each `global_alloc_copy` produces an owned movable
    // global handle (or null on alloc failure). We collect all
    // three before opening the clipboard so a partial-failure
    // path can free everything cleanly. The Open/Empty/Set/Close
    // sequence below matches the `set_clipboard_format` lifecycle;
    // each `SetClipboardData` either transfers ownership (returns
    // non-null) or fails (we free).
    unsafe {
        let plain_wide_bytes =
            core::slice::from_raw_parts(plain_wide.as_ptr() as *const u8, plain_byte_len);
        let plain_hmem = global_alloc_copy(plain_wide_bytes, false);
        // Skip the HTML / RTF allocations entirely if their
        // registered format ids are zero — no point allocating
        // for a format we can't set.
        let html_hmem = if cf_html != 0 {
            global_alloc_copy(build_cf_html(html).as_bytes(), true)
        } else {
            core::ptr::null_mut()
        };
        let rtf_hmem = if cf_rtf != 0 {
            global_alloc_copy(rtf.as_bytes(), true)
        } else {
            core::ptr::null_mut()
        };

        // If every allocation failed there's nothing to ship.
        if plain_hmem.is_null() && html_hmem.is_null() && rtf_hmem.is_null() {
            return false;
        }

        if OpenClipboard(npp) == 0 {
            if !plain_hmem.is_null() {
                GlobalFree(plain_hmem);
            }
            if !html_hmem.is_null() {
                GlobalFree(html_hmem);
            }
            if !rtf_hmem.is_null() {
                GlobalFree(rtf_hmem);
            }
            return false;
        }
        EmptyClipboard();

        let r_plain = if !plain_hmem.is_null() {
            SetClipboardData(CF_UNICODETEXT, plain_hmem)
        } else {
            core::ptr::null_mut()
        };
        let r_html = if !html_hmem.is_null() {
            SetClipboardData(cf_html, html_hmem)
        } else {
            core::ptr::null_mut()
        };
        let r_rtf = if !rtf_hmem.is_null() {
            SetClipboardData(cf_rtf, rtf_hmem)
        } else {
            core::ptr::null_mut()
        };

        CloseClipboard();

        // Free any handles the clipboard didn't take.
        if !plain_hmem.is_null() && r_plain.is_null() {
            GlobalFree(plain_hmem);
        }
        if !html_hmem.is_null() && r_html.is_null() {
            GlobalFree(html_hmem);
        }
        if !rtf_hmem.is_null() && r_rtf.is_null() {
            GlobalFree(rtf_hmem);
        }

        // Success if at least one format landed.
        !r_plain.is_null() || !r_html.is_null() || !r_rtf.is_null()
    }
}

/// Cache the registered "Rich Text Format" clipboard id. Win32's
/// `RegisterClipboardFormat` canonicalises by name (every caller
/// that asks for the same name gets the same id) so the value is
/// stable for the process lifetime; the `OnceLock` shaves one
/// kernel transition per clipboard write after the first.
fn register_rtf_format() -> u32 {
    use std::sync::OnceLock;
    static CACHED: OnceLock<u32> = OnceLock::new();
    *CACHED.get_or_init(|| unsafe { RegisterClipboardFormatA(c"Rich Text Format".as_ptr()) })
}

/// Same shape as [`register_rtf_format`] but for the `CF_HTML`
/// registered format. The exact name "HTML Format" is canonical
/// per Microsoft's CF_HTML spec; Word, Outlook, and Chromium-
/// based editors all key on this string.
fn register_html_format() -> u32 {
    use std::sync::OnceLock;
    static CACHED: OnceLock<u32> = OnceLock::new();
    *CACHED.get_or_init(|| unsafe { RegisterClipboardFormatA(c"HTML Format".as_ptr()) })
}

/// Wrap an HTML document in CF_HTML's documented byte-offset
/// header so Word, Outlook, and CF_HTML-aware browsers can paste
/// it as styled HTML rather than raw markup.
///
/// CF_HTML's spec (Microsoft):
/// ```text
/// Version:0.9
/// StartHTML:NNNNNNNNNN
/// EndHTML:NNNNNNNNNN
/// StartFragment:NNNNNNNNNN
/// EndFragment:NNNNNNNNNN
/// <html>...
/// <body>...
/// <!--StartFragment-->[content]<!--EndFragment-->
/// </body></html>
/// ```
/// Each `NNNNNNNNNN` is a 10-digit zero-padded byte offset into
/// the entire CF_HTML payload (header included). The receiving
/// app reads the offsets to extract the "interesting" content
/// without parsing the surrounding `<html>` shell.
///
/// We inject `<!--StartFragment-->` / `<!--EndFragment-->` markers
/// into the existing `build_html` output (right inside the
/// `<body>` tags), then prepend the header with computed offsets.
fn build_cf_html(full_html: &str) -> String {
    const SF_MARKER: &str = "<!--StartFragment-->";
    const EF_MARKER: &str = "<!--EndFragment-->";
    const BODY_OPEN: &str = "<body>";
    const BODY_CLOSE: &str = "</body>";

    let html_with_markers = if let (Some(open_idx), Some(close_idx)) =
        (full_html.find(BODY_OPEN), full_html.rfind(BODY_CLOSE))
    {
        let after_open = open_idx + BODY_OPEN.len();
        let mut s = String::with_capacity(full_html.len() + SF_MARKER.len() + EF_MARKER.len());
        s.push_str(&full_html[..after_open]);
        s.push_str(SF_MARKER);
        s.push_str(&full_html[after_open..close_idx]);
        s.push_str(EF_MARKER);
        s.push_str(&full_html[close_idx..]);
        s
    } else {
        // Defensive fallback: no `<body>` tags found in build_html
        // output (shouldn't happen — build_html always emits them
        // — but a future refactor that drops them would otherwise
        // produce a malformed CF_HTML payload). Wrap the whole
        // input with the minimal required structure.
        format!("<html><body>{SF_MARKER}{full_html}{EF_MARKER}</body></html>")
    };

    // Defensive `find` rather than `.expect()` — even though both
    // markers were just injected and must be present, an
    // assertion failure here would panic across the FFI boundary
    // when this is called from a plugin menu callback. Falling
    // back to an empty payload is safer: the receiving app sees
    // no clipboard data, the plugin doesn't crash the host.
    let (sf_offset_in_html, ef_offset_in_html) = match (
        html_with_markers.find(SF_MARKER),
        html_with_markers.find(EF_MARKER),
    ) {
        (Some(sf), Some(ef)) => (sf, ef),
        _ => return String::new(),
    };

    // Build the header twice through the same helper — once with
    // placeholder zeros to measure the byte length, once with the
    // computed real offsets. Both calls share the same `format!`
    // template literal inside `cf_html_header`, so the lengths
    // are guaranteed identical (each `{:010}` field is exactly
    // 10 ASCII digits regardless of value, which is the property
    // CF_HTML's spec relies on). That sidesteps the alternative
    // — a separate const "template" string whose length had to
    // match the runtime `format!` — which can silently drift in
    // release builds where `debug_assert_eq!` is a no-op.
    let header_len = cf_html_header(0, 0, 0, 0).len();

    let start_html = header_len;
    let start_fragment = header_len + sf_offset_in_html + SF_MARKER.len();
    let end_fragment = header_len + ef_offset_in_html;
    let end_html = header_len + html_with_markers.len();

    let header = cf_html_header(start_html, end_html, start_fragment, end_fragment);
    debug_assert_eq!(
        header.len(),
        header_len,
        "CF_HTML header length drifted between sizing and emission — offsets would be off",
    );

    format!("{header}{html_with_markers}")
}

/// Format a CF_HTML header with the four offset fields. Used by
/// [`build_cf_html`] to size and to emit, ensuring both passes
/// share one format-string source of truth.
fn cf_html_header(
    start_html: usize,
    end_html: usize,
    start_fragment: usize,
    end_fragment: usize,
) -> String {
    format!(
        "Version:0.9\r\n\
         StartHTML:{start_html:010}\r\n\
         EndHTML:{end_html:010}\r\n\
         StartFragment:{start_fragment:010}\r\n\
         EndFragment:{end_fragment:010}\r\n",
    )
}

/// Allocate a `GlobalAlloc(GMEM_MOVEABLE)` block of `bytes.len()`
/// (or `bytes.len() + 1` if `nul_terminate`), copy `bytes` in, and
/// return the handle. Returns null on alloc / lock / overflow
/// failure. The caller hands the handle to `SetClipboardData`
/// (which transfers ownership) or `GlobalFree`s it.
///
/// # Safety
///
/// On success the returned `*mut c_void` is a valid `HGLOBAL`
/// owned by the caller. Caller is responsible for either passing
/// it to a successful `SetClipboardData` or `GlobalFree`-ing it.
unsafe fn global_alloc_copy(bytes: &[u8], nul_terminate: bool) -> *mut c_void {
    let alloc_size = if nul_terminate {
        match bytes.len().checked_add(1) {
            Some(n) => n,
            None => return core::ptr::null_mut(),
        }
    } else {
        bytes.len()
    };
    // SAFETY: GlobalAlloc with GMEM_MOVEABLE returns a handle (or
    // null on failure). We don't dereference until after a
    // successful Lock.
    let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, alloc_size) };
    if hmem.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: GlobalLock on a valid moveable handle returns a
    // pointer to its content (or null on failure). We free on
    // failure to keep the handle from leaking.
    let dest = unsafe { GlobalLock(hmem) };
    if dest.is_null() {
        unsafe { GlobalFree(hmem) };
        return core::ptr::null_mut();
    }
    if !bytes.is_empty() {
        // SAFETY: `dest` is valid for `alloc_size` bytes; `bytes`
        // is valid for `bytes.len()` bytes which is ≤ alloc_size.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dest as *mut u8, bytes.len());
        }
    }
    if nul_terminate {
        // SAFETY: `dest + bytes.len()` is in-bounds (alloc_size = bytes.len() + 1).
        unsafe { *(dest as *mut u8).add(bytes.len()) = 0 };
    }
    // SAFETY: pairs with the GlobalLock above. After unlock the
    // lock count returns to zero, which SetClipboardData requires.
    unsafe { GlobalUnlock(hmem) };
    hmem
}

/// Push a single clipboard format onto the clipboard. Wraps the
/// Open/Empty/Set/Close sequence around one
/// [`global_alloc_copy`]; on `SetClipboardData` failure the
/// allocation is freed. Returns `true` on success.
///
/// Known limitation (inherited from Win32): `EmptyClipboard` runs
/// before `SetClipboardData`, so a SetClipboardData failure
/// (extremely rare — only OOM or resource exhaustion) loses the
/// user's previous clipboard content. There's no API path to set
/// without first emptying.
fn set_clipboard_format(format: u32, bytes: &[u8], nul_terminate: bool) -> bool {
    let npp = sdk::npp_handle();
    // SAFETY: `global_alloc_copy` returns a handle owned by us
    // until either `SetClipboardData` accepts ownership or we
    // `GlobalFree` it on a failure path.
    unsafe {
        let hmem = global_alloc_copy(bytes, nul_terminate);
        if hmem.is_null() {
            return false;
        }
        if OpenClipboard(npp) == 0 {
            GlobalFree(hmem);
            return false;
        }
        EmptyClipboard();
        let result = SetClipboardData(format, hmem);
        CloseClipboard();
        if result.is_null() {
            GlobalFree(hmem);
            return false;
        }
        true
    }
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

    // ---- CF_HTML tests ----

    #[test]
    fn build_cf_html_emits_required_header_fields() {
        let html = "<!DOCTYPE html>\n<html>\n<body>\n<span>x</span>\n</body>\n</html>\n";
        let payload = build_cf_html(html);
        assert!(payload.starts_with("Version:0.9\r\n"));
        assert!(payload.contains("StartHTML:"));
        assert!(payload.contains("EndHTML:"));
        assert!(payload.contains("StartFragment:"));
        assert!(payload.contains("EndFragment:"));
        assert!(payload.contains("<!--StartFragment-->"));
        assert!(payload.contains("<!--EndFragment-->"));
    }

    #[test]
    fn build_cf_html_offsets_point_at_correct_positions() {
        // Parse the offsets out of the header and check they
        // identify the documented positions in the payload.
        let html = "<!DOCTYPE html>\n<html>\n<body>\n<span>x</span>\n</body>\n</html>\n";
        let payload = build_cf_html(html);

        let parse_offset = |key: &str| -> usize {
            let line_start = payload.find(key).unwrap() + key.len();
            // Each offset field is 10 ASCII digits.
            payload[line_start..line_start + 10]
                .parse::<usize>()
                .unwrap()
        };
        let start_html = parse_offset("StartHTML:");
        let end_html = parse_offset("EndHTML:");
        let start_fragment = parse_offset("StartFragment:");
        let end_fragment = parse_offset("EndFragment:");

        // StartHTML lands at the first byte of `<` in `<!DOCTYPE`.
        assert_eq!(&payload.as_bytes()[start_html..start_html + 1], b"<");
        // EndHTML is the total payload length.
        assert_eq!(end_html, payload.len());
        // StartFragment lands immediately after the marker.
        let sf_marker_end =
            payload.find("<!--StartFragment-->").unwrap() + "<!--StartFragment-->".len();
        assert_eq!(start_fragment, sf_marker_end);
        // EndFragment lands at the start of the closing marker.
        let ef_marker_start = payload.find("<!--EndFragment-->").unwrap();
        assert_eq!(end_fragment, ef_marker_start);
    }

    #[test]
    fn build_cf_html_offsets_are_zero_padded_to_10_digits() {
        // The CF_HTML spec requires exactly 10-digit zero-padded
        // offset fields. A drift in pad width would push every
        // subsequent field's byte position.
        let html = "<html><body>x</body></html>";
        let payload = build_cf_html(html);
        for key in ["StartHTML:", "EndHTML:", "StartFragment:", "EndFragment:"] {
            let pos = payload.find(key).unwrap() + key.len();
            let digits = &payload[pos..pos + 10];
            assert!(
                digits.chars().all(|c| c.is_ascii_digit()),
                "{key} should be exactly 10 digits, got {digits:?}",
            );
            // The 11th char must be `\r` (the \r\n line terminator).
            assert_eq!(payload.as_bytes()[pos + 10], b'\r');
        }
    }

    #[test]
    fn build_cf_html_falls_back_when_body_tags_missing() {
        // Defensive path: an HTML input without `<body>` tags
        // (shouldn't happen from build_html, but a future
        // refactor could break the assumption) should still
        // produce a valid CF_HTML payload by wrapping the input
        // in minimal `<html><body>` scaffolding.
        let payload = build_cf_html("<span>just a fragment</span>");
        assert!(payload.contains("<!--StartFragment-->"));
        assert!(payload.contains("just a fragment"));
        assert!(payload.contains("<!--EndFragment-->"));
        assert!(payload.contains("<html>"));
        assert!(payload.contains("</html>"));
    }

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
