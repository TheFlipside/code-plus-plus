//! Entry-point implementations for cppmimetools.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! Twenty `FuncItem` entries — seventeen commands + three separators.
//! Layout matches Notepad++'s shipped mimeTools plugin so users
//! coming from N++ see the same menu organisation:
//!
//! ```text
//!   Base64 Encode
//!   Base64 Encode with padding
//!   Base64 Encode with Unix EOL
//!   Base64 Encode by line
//!   Base64 Decode
//!   Base64 Decode strict
//!   Base64 Decode by line
//!   ---
//!   Quoted-printable Encode
//!   Quoted-printable Decode
//!   ---
//!   URL Encode (RFC1738)
//!   URL Encode (RFC1738) by line
//!   URL Encode (Extended)
//!   URL Encode (Extended) by line
//!   URL Encode (Full)
//!   URL Encode (Full) by line
//!   URL Decode
//!   ---
//!   SAML Decode
//! ```
//!
//! Separators are encoded as `FuncItem { p_func: None, .. }` — the
//! host's `build_plugin_submenu` renders any entry with no callback
//! as `MF_SEPARATOR` (see `ui_win32/src/lib.rs` near
//! `if func.p_func.is_none()`).
//!
//! Every command operates on the active Scintilla view's selection:
//! reads it as bytes, transforms it, and writes the result back. The
//! selection round-trip uses the binary-safe `SCI_SETTARGETRANGE` /
//! `SCI_REPLACETARGET` pair, so encode outputs that are pure ASCII
//! AND decode outputs that are arbitrary bytes both survive the
//! buffer write intact.

#![cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]

use codepp_plugin_sdk::{self as sdk, FuncItem, NppData, SCNotification, SyncCell};

const PLUGIN_NAME: [u16; 11] = make_plugin_name();

const fn make_plugin_name() -> [u16; 11] {
    // "MIME Tools\0" — 10 ASCII chars + NUL.
    let mut buf = [0u16; 11];
    let bytes = b"MIME Tools";
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

/// Sentinel label for separator `FuncItem` entries. The host renders
/// `MF_SEPARATOR` on any entry with `p_func: None` and ignores the
/// label — but the field can't be left uninitialised (it's an array,
/// not an `Option`), so a sentinel string lets a debugger reading the
/// raw `FuncItem` array find separator entries clearly.
const SEPARATOR_LABEL: &[u8] = b"---";

const FUNC_COUNT: usize = 20;

static FUNCS: SyncCell<[FuncItem; FUNC_COUNT]> = SyncCell::new([
    // Base64 family — 7 commands.
    FuncItem {
        item_name: sdk::menu_label(b"Base64 Encode"),
        p_func: Some(cmd_base64_encode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Base64 Encode with padding"),
        p_func: Some(cmd_base64_encode_with_padding),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Base64 Encode with Unix EOL"),
        p_func: Some(cmd_base64_encode_unix_eol),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Base64 Encode by line"),
        p_func: Some(cmd_base64_encode_by_line),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Base64 Decode"),
        p_func: Some(cmd_base64_decode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Base64 Decode strict"),
        p_func: Some(cmd_base64_decode_strict),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Base64 Decode by line"),
        p_func: Some(cmd_base64_decode_by_line),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    // --- separator ---
    FuncItem {
        item_name: sdk::menu_label(SEPARATOR_LABEL),
        p_func: None,
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    // Quoted-printable family — 2 commands.
    FuncItem {
        item_name: sdk::menu_label(b"Quoted-printable Encode"),
        p_func: Some(cmd_qp_encode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Quoted-printable Decode"),
        p_func: Some(cmd_qp_decode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    // --- separator ---
    FuncItem {
        item_name: sdk::menu_label(SEPARATOR_LABEL),
        p_func: None,
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    // URL family — 6 encode variants + 1 decode.
    FuncItem {
        item_name: sdk::menu_label(b"URL Encode (RFC1738)"),
        p_func: Some(cmd_url_encode_rfc1738),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"URL Encode (RFC1738) by line"),
        p_func: Some(cmd_url_encode_rfc1738_by_line),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"URL Encode (Extended)"),
        p_func: Some(cmd_url_encode_extended),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"URL Encode (Extended) by line"),
        p_func: Some(cmd_url_encode_extended_by_line),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"URL Encode (Full)"),
        p_func: Some(cmd_url_encode_full),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"URL Encode (Full) by line"),
        p_func: Some(cmd_url_encode_full_by_line),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"URL Decode"),
        p_func: Some(cmd_url_decode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    // --- separator ---
    FuncItem {
        item_name: sdk::menu_label(SEPARATOR_LABEL),
        p_func: None,
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    // SAML family — 1 command.
    FuncItem {
        item_name: sdk::menu_label(b"SAML Decode"),
        p_func: Some(cmd_saml_decode),
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
        unsafe { *nb = FUNC_COUNT as i32 };
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

// ---- Selection plumbing -----------------------------------------------------
//
// Every command follows the same four-step shape: fetch the
// selection, bail with a status-bar message if empty, transform, and
// write the result back. The `run_encode` / `run_decode` helpers
// collapse that boilerplate so each command-callback body fits on
// one line.

fn run_encode<F: FnOnce(&[u8]) -> Vec<u8>>(transform: F) {
    let sci = sdk::active_scintilla();
    let bytes = sdk::get_selection_bytes(sci);
    if bytes.is_empty() {
        sdk::set_status("MIME Tools: no selection");
        return;
    }
    let out = transform(&bytes);
    sdk::replace_selection(sci, &out);
}

fn run_decode<F: FnOnce(&[u8]) -> Result<Vec<u8>, &'static str>>(transform: F) {
    let sci = sdk::active_scintilla();
    let bytes = sdk::get_selection_bytes(sci);
    if bytes.is_empty() {
        sdk::set_status("MIME Tools: no selection");
        return;
    }
    match transform(&bytes) {
        Ok(out) => sdk::replace_selection(sci, &out),
        Err(msg) => sdk::set_status(&format!("MIME Tools: {msg}")),
    }
}

// ---- Menu callbacks --------------------------------------------------------

extern "C" fn cmd_base64_encode() {
    run_encode(|b| base64_encode(b, false).into_bytes());
}

extern "C" fn cmd_base64_encode_with_padding() {
    run_encode(|b| base64_encode(b, true).into_bytes());
}

extern "C" fn cmd_base64_encode_unix_eol() {
    run_encode(|b| base64_encode_unix_lines(b).into_bytes());
}

extern "C" fn cmd_base64_encode_by_line() {
    run_encode(|b| for_each_line(b, |line| base64_encode(line, false).into_bytes()));
}

extern "C" fn cmd_base64_decode() {
    run_decode(|b| base64_decode(b, B64Decode::Lenient));
}

extern "C" fn cmd_base64_decode_strict() {
    run_decode(|b| base64_decode(b, B64Decode::Strict));
}

extern "C" fn cmd_base64_decode_by_line() {
    // All-or-nothing: if any line fails to decode, the entire
    // selection is left untouched and the status bar shows the
    // first error. Rationale: a partial write would replace good
    // user content with a mix of decoded bytes and empty
    // placeholders (where the failing lines went), which is harder
    // to recover from than no write at all. The user can fix the
    // bad line and retry. The trade-off is documented as a known
    // divergence from Notepad++'s mimeTools, which writes partial
    // output — addressing that requires the run_decode contract to
    // grow a "best-effort with diagnostic" mode.
    run_decode(|b| {
        let mut err: Result<(), &'static str> = Ok(());
        let out = for_each_line(b, |line| match base64_decode_words(line) {
            Ok(v) => v,
            Err(msg) => {
                if err.is_ok() {
                    err = Err(msg);
                }
                Vec::new()
            }
        });
        err.map(|()| out)
    });
}

extern "C" fn cmd_qp_encode() {
    run_encode(|b| qp_encode(b).into_bytes());
}

extern "C" fn cmd_qp_decode() {
    run_decode(qp_decode);
}

extern "C" fn cmd_url_encode_rfc1738() {
    run_encode(|b| url_encode_rfc1738(b).into_bytes());
}

extern "C" fn cmd_url_encode_rfc1738_by_line() {
    run_encode(|b| for_each_line(b, |line| url_encode_rfc1738(line).into_bytes()));
}

extern "C" fn cmd_url_encode_extended() {
    run_encode(|b| url_encode_extended(b).into_bytes());
}

extern "C" fn cmd_url_encode_extended_by_line() {
    run_encode(|b| for_each_line(b, |line| url_encode_extended(line).into_bytes()));
}

extern "C" fn cmd_url_encode_full() {
    run_encode(|b| url_encode_full(b).into_bytes());
}

extern "C" fn cmd_url_encode_full_by_line() {
    run_encode(|b| for_each_line(b, |line| url_encode_full(line).into_bytes()));
}

extern "C" fn cmd_url_decode() {
    run_decode(url_decode);
}

extern "C" fn cmd_saml_decode() {
    run_decode(saml_decode);
}

// ---- "By line" helper ------------------------------------------------------
//
// Many MIME tools traditionally operate per-line so that a user can
// select multiple data items at once. The helper walks the input
// splitting on LF or CRLF, applies `f` to each line's content
// (without the EOL bytes), and re-emits the original EOL bytes
// verbatim between transformed lines. A trailing line without an EOL
// is preserved as such.

fn for_each_line<F: FnMut(&[u8]) -> Vec<u8>>(bytes: &[u8], mut f: F) -> Vec<u8> {
    // Rough capacity guess — encode usually grows by ~33% (base64) or
    // ~3x (URL encoding %xx triples), decode shrinks. Start with the
    // input length and let `Vec` grow as needed.
    let mut out = Vec::with_capacity(bytes.len());
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            // Line content stops before any \r that precedes the \n,
            // so `cmd a\r\n` transforms "a" and the EOL is "\r\n".
            let content_end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            out.extend_from_slice(&f(&bytes[start..content_end]));
            out.extend_from_slice(&bytes[content_end..=i]);
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        out.extend_from_slice(&f(&bytes[start..]));
    }
    out
}

// ---- Base64 (RFC 4648, standard alphabet) ---------------------------------
//
// The encoders and decoders follow the upstream mimeTools, measured
// command by command against the one Notepad++ 8.9.8.1 ships:
//
// "Base64 Encode"                unpadded, one line.
// "Base64 Encode with padding"   padded, one line.
// "Base64 Encode with Unix EOL"  padded, a `\n` after every 64 output
//                                characters, none after the last line.
// "Base64 Encode by line"        each line on its own, unpadded.
//
// Both decoders skip whitespace anywhere, which to upstream is a space,
// a tab, CR and LF only (a vertical tab or a form feed is a stray
// byte), and take the encoders' output back, padded or not: `=` padding
// is optional, and any amount of it is accepted at the end. A single
// character left over after the last group of four carries too few
// bits for a byte and is refused. A selection of nothing but whitespace
// decodes to nothing, so the command empties it, as upstream does.
//
// "Base64 Decode strict" refuses, as upstream does: `=` anywhere but
// after a partial group of four (so `YQ==` decodes and `aGVs=` and `==`
// do not), data after `=`, and any byte outside the alphabet.
// "Base64 Decode" skips `=` wherever it appears, as upstream does, so
// two padded encodings run together decode as one, and a selection of
// nothing but `=` decodes to nothing. It still refuses a byte outside
// the alphabet, and the lone leftover character, where upstream copies
// the stray byte into its output and makes a byte of the six bits: the
// selection is then left as it was rather than replaced with text that
// decodes nothing.
// "Base64 Decode by line" decodes each run of non-whitespace on a line
// on its own, as "Base64 Decode" does, and keeps the whitespace.

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// Output characters per line of "Base64 Encode with Unix EOL".
const B64_LINE_WIDTH: usize = 64;

fn base64_encode(bytes: &[u8], pad: bool) -> String {
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let v = chunk
            .iter()
            .enumerate()
            .fold(0u32, |v, (i, &b)| v | (u32::from(b) << (16 - 8 * i)));
        // A chunk of n bytes is n + 1 characters, padded to four.
        for i in 0..=chunk.len() {
            s.push(B64_ALPHABET[((v >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
        if pad {
            for _ in chunk.len()..3 {
                s.push('=');
            }
        }
    }
    s
}

fn base64_encode_unix_lines(bytes: &[u8]) -> String {
    let s = base64_encode(bytes, true);
    // Base64 is ASCII, so a character is a byte and `i` counts output
    // characters.
    let mut out = String::with_capacity(s.len() + s.len() / B64_LINE_WIDTH);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && i.is_multiple_of(B64_LINE_WIDTH) {
            out.push('\n');
        }
        out.push(c);
    }
    out
}

/// A line for "Base64 Decode by line": each run of non-whitespace is an
/// encoding of its own, decoded as "Base64 Decode" decodes it, and the
/// whitespace between runs is kept, as upstream keeps it (`aGVs bG8=`
/// is `hel lo`).
fn base64_decode_words(line: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(line.len());
    let mut rest = line;
    while let Some(&first) = rest.first() {
        let space = b64_space(first);
        let run = rest
            .iter()
            .position(|&b| b64_space(b) != space)
            .unwrap_or(rest.len());
        let (piece, tail) = rest.split_at(run);
        if space {
            out.extend_from_slice(piece);
        } else {
            out.extend(base64_decode(piece, B64Decode::Lenient)?);
        }
        rest = tail;
    }
    Ok(out)
}

/// Which of the two decoders: see the section comment above.
#[derive(Clone, Copy, PartialEq, Eq)]
enum B64Decode {
    Strict,
    Lenient,
}

fn base64_decode(bytes: &[u8], mode: B64Decode) -> Result<Vec<u8>, &'static str> {
    let mut digits = Vec::with_capacity(bytes.len());
    let mut padded = false;
    for &b in bytes {
        if b64_space(b) {
            continue;
        }
        if b == b'=' {
            if mode == B64Decode::Strict && digits.len() % 4 == 0 {
                return Err("base64 strict: padding that follows no partial group");
            }
            padded = true;
            continue;
        }
        let d = b64_digit(b).ok_or("invalid base64 character")?;
        if padded && mode == B64Decode::Strict {
            return Err("base64 strict: padding before the end of the input");
        }
        digits.push(d);
    }
    if digits.len() % 4 == 1 {
        return Err("base64: one character too many to make a byte");
    }
    let mut out = Vec::with_capacity(digits.len() / 4 * 3 + 2);
    for group in digits.chunks(4) {
        let v = group
            .iter()
            .enumerate()
            .fold(0u32, |v, (i, &d)| v | (u32::from(d) << (18 - 6 * i)));
        // A group of n characters makes n - 1 bytes; the bits left over
        // are dropped, as upstream drops them.
        for i in 0..group.len() - 1 {
            out.push(((v >> (16 - 8 * i)) & 0xff) as u8);
        }
    }
    Ok(out)
}

/// Whitespace to the Base64 decoders: what upstream skips, measured.
fn b64_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn b64_digit(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

// ---- URL encoding ----------------------------------------------------------
//
// Three encode variants differing in which bytes pass through literally:
//
// * RFC1738 — the historical "URL character set" (RFC 1738 §2.2). Safe
//   set is alphanumerics + `$ - _ . + ! * ' ( ) ,`. The most permissive
//   of the three: leaves the most punctuation literal, producing the
//   most readable output for plain-ASCII inputs.
//
// * Extended — encode any non-alphanumeric byte. The set most often
//   wanted when stuffing arbitrary data into a query-string value where
//   any punctuation might be reserved by the application. Equivalent to
//   `encodeURIComponent` minus the RFC 3986 unreserved punctuation
//   (`-_.~`), which it ALSO encodes.
//
// * Full — encode every byte as `%XX`, even `A`-`Z`. Useful for
//   round-tripping arbitrary binary through systems that interpret any
//   unencoded character. Output length is exactly 3x the input length.
//
// All three share `url_decode` — `%XX` decoding doesn't depend on
// which encoder produced the input, and `+` is passed through unchanged
// (Code++ buffers commonly contain literal `+` in source code, and form-
// encoding's "+ means space" convention would corrupt that).

fn url_encode_with<F: Fn(u8) -> bool>(bytes: &[u8], is_safe: F) -> String {
    // Worst case: every byte → 3 chars (`%XX`). Reserve that to avoid
    // realloc churn on inputs of any size. `saturating_mul` guards
    // against overflow on 32-bit targets — Code++'s shipped targets
    // are 64-bit today, but the plugin ABI mentions 32-bit support
    // and a saturated capacity hint degrades gracefully (the `String`
    // reallocates as needed) where a wrapped value would feed a
    // bogus value into the allocator.
    let mut s = String::with_capacity(bytes.len().saturating_mul(3));
    for &b in bytes {
        if is_safe(b) {
            s.push(char::from(b));
        } else {
            s.push('%');
            s.push(HEX_UPPER[(b >> 4) as usize] as char);
            s.push(HEX_UPPER[(b & 0x0f) as usize] as char);
        }
    }
    s
}

fn url_encode_rfc1738(bytes: &[u8]) -> String {
    url_encode_with(bytes, is_rfc1738_safe)
}

fn url_encode_extended(bytes: &[u8]) -> String {
    url_encode_with(bytes, |b| b.is_ascii_alphanumeric())
}

fn url_encode_full(bytes: &[u8]) -> String {
    url_encode_with(bytes, |_| false)
}

fn is_rfc1738_safe(b: u8) -> bool {
    // RFC 1738 §2.2: alphanumerics + the "mark" set
    // `$ - _ . + ! * ' ( ) ,`. (RFC 3986 §2.3 later trimmed this to
    // `- _ . ~` — we deliberately use the older, more permissive set
    // here because the "RFC1738" label promises it.)
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'$' | b'-' | b'_' | b'.' | b'+' | b'!' | b'*' | b'\'' | b'(' | b')' | b','
        )
}

fn url_decode(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err("URL decode: truncated %xx escape");
            }
            let hi = hex_digit(bytes[i + 1]).ok_or("URL decode: invalid hex digit after %")?;
            let lo = hex_digit(bytes[i + 2]).ok_or("URL decode: invalid hex digit after %")?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ---- Quoted-Printable (RFC 2045 §6.7) -------------------------------------
//
// The encoder writes what upstream mimeTools writes, measured byte for
// byte against the one Notepad++ 8.9.8.1 ships, through the plugin
// harness:
//
//   * **75 columns, then a soft break.** A line holds at most 75
//     characters before the soft break `=\r\n` (0x3D 0x0D 0x0A, on
//     every platform), so with its `=` it is at most 76 long, RFC 2045
//     Rule 5's limit. A byte that does not fit on the line goes after a
//     soft break, and an `=XX` triple is never split by one.
//
//   * **Line breaks stay as they are.** An LF is written as it is,
//     with no soft break before it, and starts a line at column 1, not
//     0: upstream's count, so a line after an LF holds 74 characters.
//     A CR is an ordinary literal character: it takes a column and can
//     be pushed past a soft break like any other. This is RFC 2045's
//     text mode; until Phase 5 the encoder wrote `=0D=0A` for a CRLF
//     (binary mode), which no Notepad++ user would have got.
//
//   * **Trailing whitespace stays as it is.** A space or tab at the
//     end of a line or of the input is written literally, as upstream
//     writes it, where Rule 3 asks for `=20` / `=09`. A decoder that
//     strips trailing whitespace, as the RFC lets it, loses it; this
//     plugin's decoder keeps it.
//
// What is literal: tab, CR, space, and the printable ASCII bytes but
// `=` (33..=60 and 62..=126). Everything else, `=` and every byte
// above 0x7E included, is `=XX`, in upper case.

/// The most characters on an encoded line before its soft break: RFC
/// 2045's 76-character limit less the soft break's `=`.
const QP_PAYLOAD_CAP: usize = 75;

fn qp_encode(bytes: &[u8]) -> String {
    // Worst case ~3.12x expansion: every byte → `=XX` triple PLUS
    // one `=\r\n` soft break per 25 encoded chars. `saturating_mul(3)`
    // gets close enough to avoid most reallocations and matches the
    // existing `url_encode_with` capacity convention.
    let mut out = String::with_capacity(bytes.len().saturating_mul(3));
    let mut line_len = 0usize;
    for &b in bytes {
        if b == b'\n' {
            out.push('\n');
            // Upstream counts the LF as the new line's first column.
            line_len = 1;
        } else if is_qp_safe(b) {
            qp_emit_literal(&mut out, &mut line_len, b);
        } else {
            qp_emit_encoded(&mut out, &mut line_len, b);
        }
    }
    out
}

/// Emit a 1-char literal byte, inserting a soft break first if the
/// current line is already at the 75-char payload cap.
fn qp_emit_literal(out: &mut String, line_len: &mut usize, b: u8) {
    if *line_len + 1 > QP_PAYLOAD_CAP {
        out.push_str("=\r\n");
        *line_len = 0;
    }
    out.push(char::from(b));
    *line_len += 1;
}

/// Emit a 3-char `=XX` triple, inserting a soft break first if the
/// current line can't fit the full triple within the 75-char payload
/// cap. The triple is atomic per RFC 2045 §6.7 Rule 1 — never split.
fn qp_emit_encoded(out: &mut String, line_len: &mut usize, b: u8) {
    if *line_len + 3 > QP_PAYLOAD_CAP {
        out.push_str("=\r\n");
        *line_len = 0;
    }
    out.push('=');
    out.push(HEX_UPPER[(b >> 4) as usize] as char);
    out.push(HEX_UPPER[(b & 0x0f) as usize] as char);
    *line_len += 3;
}

/// Whether the encoder writes `b` as it is: upstream's set, measured.
/// An LF is written as it is too, but `qp_encode` handles it apart,
/// since it starts a line rather than taking a column.
fn is_qp_safe(b: u8) -> bool {
    matches!(b, b'\t' | b'\r' | b' ' | 33..=60 | 62..=126)
}

/// `=XX` to its byte, a soft break (`=` and a CRLF, or `=` and an LF) to
/// nothing, and every other byte as it is. Only `=` followed by neither
/// two hex digits nor a line break is refused.
///
/// More lenient than upstream mimeTools, which refuses lower-case hex
/// (`=c3`), an LF-only soft break, and any CR or LF that is not part of
/// a CRLF, with "It's not a valid Quoted-printable text" (measured): RFC
/// 2045 asks a robust decoder to read the first, and a file with LF line
/// ends holds the others. Upstream's decoder therefore refuses what its
/// own encoder writes for such a file, where this one reads it back.
/// On the 28 decode inputs given to both, every one upstream read, this
/// read the same.
fn qp_decode(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            // Soft line break: `=` followed by CRLF or LF — drop
            // the entire 2- or 3-byte sequence.
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            if i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
                i += 3;
                continue;
            }
            if i + 2 >= bytes.len() {
                return Err("QP decode: truncated =XX escape");
            }
            let hi = hex_digit(bytes[i + 1]).ok_or("QP decode: invalid hex digit after =")?;
            let lo = hex_digit(bytes[i + 2]).ok_or("QP decode: invalid hex digit after =")?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

// ---- SAML Decode -----------------------------------------------------------
//
// SAML messages travel over two HTTP bindings:
//   * Redirect Binding — URL-encoded + Base64-encoded + raw-DEFLATE
//     compressed payload riding on the query string.
//   * POST Binding — Base64-encoded XML (no compression, no URL
//     encoding) submitted as a form field.
//
// `saml_decode` covers both. The decode pipeline is:
//   1. URL decode — peels off the query-string encoding. POST-binding
//      input has nothing to decode and passes through unchanged.
//   2. Base64 decode, as "Base64 Decode" does: whitespace and `=`
//      are skipped wherever they appear, which strips any line
//      wrapping the SAML tooling may have inserted. (Until the decoder
//      followed upstream's, this step also refused misplaced padding
//      and a length not a multiple of four.)
//   3. Attempt raw-DEFLATE inflate. Redirect-binding payloads
//      inflate to XML; POST-binding payloads were already XML at
//      step 2 and inflate fails — in that case we return the step-2
//      output unchanged.
//
// The heuristic for "use the inflated output" is whether step 3
// succeeds at all. SAML payloads are XML; the chance of a non-SAML
// payload accidentally inflating cleanly is vanishingly small, and
// the worst case (false positive) is the user seeing whatever the
// inflated bytes happen to be, which is still useful diagnostic
// data.

/// Hard cap on the inflated SAML payload. Real-world SAML assertions
/// are a few KB; legitimate redirect-binding payloads stay well under
/// a megabyte. 16 MiB is roughly four orders of magnitude above any
/// legitimate value, so a refusal here means the input is either
/// malformed or a deliberate decompression bomb (DEFLATE expansion
/// can exceed 1000:1, so an attacker who got a user to paste a small
/// crafted base64 blob could otherwise inflate to multi-GB output —
/// which on alloc-failure aborts the editor and loses every tab's
/// unsaved work, since plugins are in-process per DESIGN.md §6.5).
const SAML_INFLATE_CAP: usize = 16 * 1024 * 1024;

fn saml_decode(input: &[u8]) -> Result<Vec<u8>, &'static str> {
    use miniz_oxide::inflate::TINFLStatus;
    let url_decoded = url_decode(input)?;
    let b64_decoded = base64_decode(&url_decoded, B64Decode::Lenient)?;
    // Bounded inflate. `decompress_to_vec_with_limit` returns
    // `Err(TINFLStatus)` for every failure mode: malformed input,
    // truncated stream, AND output-cap exceeded. We want to
    // distinguish "this isn't compressed (POST binding)" from "this
    // IS compressed but exceeds the cap (bomb)" so the user sees a
    // meaningful diagnostic for the bomb case rather than silently
    // getting the un-inflated bytes.
    match miniz_oxide::inflate::decompress_to_vec_with_limit(&b64_decoded, SAML_INFLATE_CAP) {
        Ok(inflated) => Ok(inflated),
        Err(e) if e.status == TINFLStatus::HasMoreOutput => {
            Err("SAML decode: DEFLATE payload exceeds 16 MiB cap (decompression bomb?)")
        }
        // Any other inflate failure — POST-binding (no compression),
        // malformed compressed stream, etc. Return the
        // base64-decoded bytes as-is: POST binding payloads ARE
        // valid XML at this stage, and a malformed-DEFLATE payload
        // is at least useful diagnostic data for the user.
        Err(_) => Ok(b64_decoded),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Base64 ------------------------------------------------------------

    #[test]
    fn base64_encode_empty() {
        assert_eq!(base64_encode(b"", true), "");
        assert_eq!(base64_encode(b"", false), "");
    }

    #[test]
    fn base64_encode_rfc4648_test_vectors() {
        // The canonical test vectors from RFC 4648 §10, padded, and
        // without the padding as "Base64 Encode" writes them.
        for (data, padded) in [
            (&b"f"[..], "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(data, true), padded);
            assert_eq!(base64_encode(data, false), padded.trim_end_matches('='));
        }
    }

    #[test]
    fn base64_encodes_as_upstream_mimetools_does() {
        // Measured against the mimeTools Notepad++ 8.9.8.1 ships, on
        // the text "hello".
        assert_eq!(base64_encode(b"hello", false), "aGVsbG8");
        assert_eq!(base64_encode(b"hello", true), "aGVsbG8=");
        assert_eq!(base64_encode_unix_lines(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn base64_unix_lines_break_every_64_characters() {
        // 60 bytes → 80 characters: one break, after the 64th, and none
        // after the last line.
        let encoded = base64_encode_unix_lines(&[b'A'; 60]);
        let lines: Vec<&str> = encoded.split('\n').collect();
        assert_eq!(lines.iter().map(|l| l.len()).collect::<Vec<_>>(), [64, 16]);
        // 48 bytes → exactly 64 characters: no break at all.
        assert!(!base64_encode_unix_lines(&[b'A'; 48]).contains('\n'));
        // The padding stays on the last line.
        assert!(base64_encode_unix_lines(&[b'A'; 49]).ends_with("\nQQ=="));
    }

    #[test]
    fn base64_decode_rfc4648_test_vectors() {
        for mode in [B64Decode::Strict, B64Decode::Lenient] {
            for (text, data) in [
                (&b""[..], &b""[..]),
                (b"Zg==", b"f"),
                (b"Zm8=", b"fo"),
                (b"Zm9v", b"foo"),
                (b"Zm9vYg==", b"foob"),
                (b"Zm9vYmE=", b"fooba"),
                (b"Zm9vYmFy", b"foobar"),
            ] {
                assert_eq!(base64_decode(text, mode).unwrap(), data);
            }
        }
    }

    #[test]
    fn base64_round_trip_random_lengths() {
        for len in 0..=100usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 13) as u8).collect();
            for encoded in [
                base64_encode(&data, false),
                base64_encode(&data, true),
                base64_encode_unix_lines(&data),
            ] {
                for mode in [B64Decode::Strict, B64Decode::Lenient] {
                    let decoded = base64_decode(encoded.as_bytes(), mode).unwrap();
                    assert_eq!(decoded, data, "round trip failed at length {len}");
                }
            }
        }
    }

    #[test]
    fn base64_decoders_skip_whitespace_and_take_any_padding_at_the_end() {
        // Each measured against the upstream "Base64 Decode strict",
        // which agrees with "Base64 Decode" on all of them.
        for mode in [B64Decode::Strict, B64Decode::Lenient] {
            let decode = |text: &[u8]| base64_decode(text, mode).unwrap();
            assert_eq!(decode(b"aGVsbG8"), b"hello");
            assert_eq!(decode(b"aGVsbG8=="), b"hello");
            assert_eq!(decode(b"aGVsbG8===="), b"hello");
            assert_eq!(decode(b"aGVsbA="), b"hell");
            assert_eq!(decode(b"aGV"), b"he");
            assert_eq!(decode(b"aGVs bG8="), b"hello");
            assert_eq!(decode(b"aGVs\nbG8 ="), b"hello");
            assert_eq!(decode(b"Z\tm\r\n9 v Y m F y"), b"foobar");
            // The bits left over past the last whole byte are dropped,
            // set or not.
            assert_eq!(decode(b"aGVsbG9"), b"hello");
        }
    }

    #[test]
    fn base64_decoders_refuse_stray_bytes_and_a_lone_character() {
        for mode in [B64Decode::Strict, B64Decode::Lenient] {
            assert!(base64_decode(b"!!!!", mode).is_err());
            assert!(base64_decode(b"aGVs!bG8=", mode).is_err());
            assert!(base64_decode(&[b'a', 0x80, b'b'], mode).is_err());
            // Upstream refuses this in strict mode and makes "h" of it
            // in the other; Code++ refuses it in both.
            assert!(base64_decode(b"a", mode).is_err());
            assert!(base64_decode(b"aGVsb", mode).is_err());
        }
    }

    #[test]
    fn base64_strict_refuses_padding_before_the_end_and_lenient_skips_it() {
        // Measured: upstream's strict decoder refuses each of these, and
        // its other decoder skips the `=`.
        for (text, lenient) in [
            (&b"=aGVs"[..], &b"hel"[..]),
            (b"aGVs=bG8", b"hello"),
            (b"aGVsbG8=a", b"hello\x1a"),
        ] {
            assert!(base64_decode(text, B64Decode::Strict).is_err());
            assert_eq!(base64_decode(text, B64Decode::Lenient).unwrap(), lenient);
        }
        // Two padded encodings run together decode as one stream, as
        // upstream decodes them: `aGVsbG8aGVs`.
        assert_eq!(
            base64_decode(b"aGVsbG8=aGVs", B64Decode::Lenient).unwrap(),
            b"hello\x1a\x19["
        );
    }

    #[test]
    fn base64_by_line_decodes_each_word_and_keeps_the_whitespace() {
        // Measured: upstream turns `aGVs bG8=` into `hel lo`.
        assert_eq!(base64_decode_words(b"aGVs bG8=").unwrap(), b"hel lo");
        assert_eq!(base64_decode_words(b" YQ\t\tYg  ").unwrap(), b" a\t\tb  ");
        assert_eq!(base64_decode_words(b"").unwrap(), b"");
        assert!(base64_decode_words(b"YQ a").is_err());
    }

    #[test]
    fn base64_whitespace_is_space_tab_cr_and_lf_only() {
        // Measured: upstream skips these four, and takes a vertical tab
        // or a form feed for a stray byte, which both decoders refuse.
        for mode in [B64Decode::Strict, B64Decode::Lenient] {
            assert_eq!(base64_decode(b"aGVs \t\r\nbG8=", mode).unwrap(), b"hello");
            assert!(base64_decode(b"YQ\x0bYg", mode).is_err());
            assert!(base64_decode(b"YQ\x0cYg", mode).is_err());
        }
        assert!(base64_decode_words(b"YQ\x0cYg").is_err());
    }

    #[test]
    fn base64_strict_takes_padding_only_after_a_partial_group() {
        // Measured against upstream's strict decoder.
        for (text, data) in [
            (&b"YQ="[..], &b"a"[..]),
            (b"YQ===", b"a"),
            (b"YQ== =", b"a"),
            (b"YWI====", b"ab"),
        ] {
            assert_eq!(base64_decode(text, B64Decode::Strict).unwrap(), data);
        }
        for text in [&b"="[..], b"====", b" =", b"aGVs=", b"aGVs===="] {
            assert!(base64_decode(text, B64Decode::Strict).is_err());
        }
        // The other decoder skips the padding: "====" is measured, the
        // rest follow from the rule.
        assert_eq!(base64_decode(b"====", B64Decode::Lenient).unwrap(), b"");
        assert_eq!(
            base64_decode(b"aGVs====", B64Decode::Lenient).unwrap(),
            b"hel"
        );
    }

    #[test]
    fn base64_whitespace_alone_decodes_to_nothing() {
        // Measured: upstream empties a selection of nothing but
        // whitespace, in both modes.
        for mode in [B64Decode::Strict, B64Decode::Lenient] {
            assert_eq!(base64_decode(b"   ", mode).unwrap(), b"");
        }
    }

    #[test]
    fn base64_round_trips_utf8_text() {
        let text = "héllo, 日本".as_bytes();
        for encoded in [
            base64_encode(text, false),
            base64_encode(text, true),
            base64_encode_unix_lines(text),
        ] {
            assert_eq!(
                base64_decode(encoded.as_bytes(), B64Decode::Strict).unwrap(),
                text
            );
        }
    }

    #[test]
    fn base64_encodes_high_bytes() {
        assert_eq!(base64_encode(&[0xff, 0xff, 0xff], true), "////");
        assert_eq!(base64_encode(&[0x00, 0x00, 0x00], true), "AAAA");
        assert_eq!(base64_encode(&[0xff], false), "/w");
    }

    // ---- "by line" ---------------------------------------------------------

    #[test]
    fn for_each_line_preserves_lf_eol() {
        // Three lines separated by LF — each transformed identically.
        let out = for_each_line(b"a\nb\nc", <[u8]>::to_vec);
        assert_eq!(out, b"a\nb\nc");
    }

    #[test]
    fn for_each_line_preserves_crlf_eol() {
        let out = for_each_line(b"a\r\nb\r\nc", <[u8]>::to_vec);
        assert_eq!(out, b"a\r\nb\r\nc");
    }

    #[test]
    fn for_each_line_transforms_each_line_independently() {
        // Upper-case each line — pin that EOL bytes pass through
        // untouched while the content gets transformed.
        let out = for_each_line(b"foo\nbar\nbaz", <[u8]>::to_ascii_uppercase);
        assert_eq!(out, b"FOO\nBAR\nBAZ");
    }

    #[test]
    fn for_each_line_trailing_eol_preserved() {
        // Input ending with EOL → output ends with the same EOL,
        // no spurious trailing transform of an empty line.
        let out = for_each_line(b"a\n", <[u8]>::to_vec);
        assert_eq!(out, b"a\n");
    }

    #[test]
    fn for_each_line_empty_input_no_transform_call() {
        // Pin: empty input → empty output, and `f` is never invoked
        // (no spurious transform of a phantom empty line).
        let mut call_count = 0;
        let out = for_each_line(b"", |line| {
            call_count += 1;
            line.to_vec()
        });
        assert!(out.is_empty());
        assert_eq!(call_count, 0);
    }

    #[test]
    fn for_each_line_lone_cr_is_content() {
        // Pin the policy: a bare `\r` without a following `\n` is
        // treated as part of the line's content, NOT as a line
        // terminator. Code++ tracks LF and CRLF as its two EOL
        // forms (`core::eol::Eol`); old-Mac CR-only line endings are
        // out of scope. Matters when transforming a buffer that
        // happens to contain a literal `\r` inside otherwise-LF or
        // CRLF data — the `\r` survives the per-line transform.
        let out = for_each_line(b"a\rb\nc", <[u8]>::to_ascii_uppercase);
        assert_eq!(out, b"A\rB\nC");
    }

    #[test]
    fn base64_encode_by_line_round_trip() {
        // Each line encodes to its own base64 chunk; the EOL bytes
        // remain literal between them. Critical for users selecting
        // a column of data items they want individually encoded.
        let input = b"foo\nbar";
        let encoded = for_each_line(input, |line| base64_encode(line, true).into_bytes());
        assert_eq!(encoded, b"Zm9v\nYmFy");
    }

    // ---- URL ---------------------------------------------------------------

    #[test]
    fn url_encode_rfc1738_passes_unreserved_through() {
        // The RFC 1738 unreserved set includes `$ - _ . + ! * ' ( ) ,`
        // alongside alphanumerics.
        assert_eq!(
            url_encode_rfc1738(b"abc123$-_.+!*'(),"),
            "abc123$-_.+!*'(),"
        );
    }

    #[test]
    fn url_encode_rfc1738_encodes_reserved() {
        // `/?&=#` etc. are NOT in the RFC 1738 unreserved set.
        assert_eq!(url_encode_rfc1738(b"a/b?c=d&e"), "a%2Fb%3Fc%3Dd%26e");
    }

    #[test]
    fn url_encode_rfc1738_encodes_space() {
        assert_eq!(url_encode_rfc1738(b"hello world"), "hello%20world");
    }

    #[test]
    fn url_encode_extended_encodes_more() {
        // "Extended" encodes ALL non-alphanumeric, including the
        // RFC 1738 punctuation that the basic encoder leaves literal.
        assert_eq!(url_encode_extended(b"foo+bar"), "foo%2Bbar");
        assert_eq!(url_encode_extended(b"abc-def"), "abc%2Ddef");
        // Alphanumerics still pass through.
        assert_eq!(url_encode_extended(b"abcXYZ123"), "abcXYZ123");
    }

    #[test]
    fn url_encode_full_encodes_everything() {
        // Full encoder: every byte becomes %XX, even letters and digits.
        assert_eq!(url_encode_full(b"abc"), "%61%62%63");
        assert_eq!(url_encode_full(b"123"), "%31%32%33");
        assert_eq!(url_encode_full(b"!"), "%21");
    }

    #[test]
    fn url_encode_full_length_is_exactly_3x() {
        // Pin the 3x length invariant — useful when sizing buffers.
        for len in 0..=20 {
            let input: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(7)).collect();
            let encoded = url_encode_full(&input);
            assert_eq!(encoded.len(), input.len() * 3);
        }
    }

    #[test]
    fn url_encode_high_bytes() {
        // `é` in UTF-8 is 0xC3 0xA9 — all three encoders agree on
        // the encoding of bytes outside the ASCII range.
        assert_eq!(url_encode_rfc1738(&[0xc3, 0xa9]), "%C3%A9");
        assert_eq!(url_encode_extended(&[0xc3, 0xa9]), "%C3%A9");
        assert_eq!(url_encode_full(&[0xc3, 0xa9]), "%C3%A9");
    }

    #[test]
    fn url_decode_round_trip_each_variant() {
        let cases: &[&[u8]] = &[
            b"hello world",
            b"a/b?c=d&e",
            b"plain",
            &[0xc3, 0xa9, 0x20, 0x66, 0x6f, 0x6f],
        ];
        for case in cases {
            for encoder in [
                url_encode_rfc1738 as fn(&[u8]) -> String,
                url_encode_extended,
                url_encode_full,
            ] {
                let encoded = encoder(case);
                let decoded = url_decode(encoded.as_bytes()).unwrap();
                assert_eq!(&decoded, case);
            }
        }
    }

    #[test]
    fn url_decode_accepts_lowercase_hex() {
        assert_eq!(url_decode(b"%c3%a9").unwrap(), vec![0xc3, 0xa9]);
    }

    #[test]
    fn url_decode_passes_plus_through() {
        // `+` is unchanged — form-encoding's "+ means space" rule
        // would corrupt source code that contains literal `+`.
        assert_eq!(url_decode(b"a+b").unwrap(), b"a+b");
    }

    #[test]
    fn url_decode_rejects_truncated_escape() {
        assert!(url_decode(b"%").is_err());
        assert!(url_decode(b"%2").is_err());
    }

    #[test]
    fn url_decode_rejects_invalid_hex() {
        assert!(url_decode(b"%ZZ").is_err());
        assert!(url_decode(b"%2Z").is_err());
    }

    // ---- Quoted-Printable --------------------------------------------------

    #[test]
    fn qp_encode_passthrough_printable() {
        assert_eq!(qp_encode(b"Hello!"), "Hello!");
    }

    #[test]
    fn qp_encode_equals_sign() {
        assert_eq!(qp_encode(b"a = b"), "a =3D b");
    }

    #[test]
    fn qp_encode_high_bytes() {
        assert_eq!(qp_encode(&[0xc3, 0xa9]), "=C3=A9");
    }

    #[test]
    fn qp_encode_control_chars() {
        // Measured against upstream mimeTools: line breaks are written
        // as they are; every other control byte, and DEL, is `=XX`.
        assert_eq!(qp_encode(b"\n"), "\n");
        assert_eq!(qp_encode(b"\r"), "\r");
        assert_eq!(qp_encode(b"\x00\x0b\x0c\x1b\x7f"), "=00=0B=0C=1B=7F");
    }

    #[test]
    fn qp_encode_tab_and_space_are_literal_everywhere() {
        // Upstream writes them as they are, at the end of a line and of
        // the input too (measured), where RFC 2045 Rule 3 asks for
        // `=20` / `=09` there.
        assert_eq!(qp_encode(b"a b\tc"), "a b\tc");
        assert_eq!(qp_encode(b" "), " ");
        assert_eq!(qp_encode(b"\t"), "\t");
        assert_eq!(qp_encode(b"a\t\t"), "a\t\t");
        assert_eq!(qp_encode(b"foo  \r\nbar"), "foo  \r\nbar");
    }

    #[test]
    fn qp_decode_basic_escape() {
        assert_eq!(qp_decode(b"a =3D b").unwrap(), b"a = b");
    }

    #[test]
    fn qp_decode_high_bytes() {
        assert_eq!(qp_decode(b"=C3=A9").unwrap(), vec![0xc3, 0xa9]);
    }

    #[test]
    fn qp_decode_lowercase_hex() {
        assert_eq!(qp_decode(b"=c3=a9").unwrap(), vec![0xc3, 0xa9]);
    }

    #[test]
    fn qp_decode_soft_break_lf() {
        assert_eq!(qp_decode(b"foo=\nbar").unwrap(), b"foobar");
    }

    #[test]
    fn qp_decode_soft_break_crlf() {
        assert_eq!(qp_decode(b"foo=\r\nbar").unwrap(), b"foobar");
    }

    /// RFC 2045 §6.7 Rule 5 invariant: every encoded line must be at
    /// most 76 chars, its line end not counted and a soft break's `=`
    /// counted. A line ends at every LF, soft break or not, and a CR
    /// before its LF is its line end; a CR elsewhere is a character of
    /// the line. This is the RFC's measure. The encoder's own count is
    /// stricter, since it also counts a CR it writes before an LF, so a
    /// line it writes is at most 75 by this helper's measure when it ends
    /// in such a CR. This helper makes the RFC's cap a machine-checked
    /// property of every wrapping test below.
    fn assert_qp_line_cap(encoded: &str) {
        for line in encoded.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            assert!(
                line.len() <= 76,
                "QP encoded line exceeds 76 chars ({} chars): {line:?}",
                line.len(),
            );
        }
    }

    #[test]
    fn qp_encode_users_canonical_example() {
        // The exact input + expected output the user supplied. The
        // input is 99 chars and contains a literal `=`. Expected
        // behaviour: encode `=` as `=3D`, then continue emitting
        // literals until column 75 is reached (at "bea"), then
        // soft-break and continue on a new line. For this input that is
        // what Python's `quopri.encodestring(..., quotetabs=False)` and
        // Perl's `MIME::QuotedPrint::encode_qp` write too; they differ
        // from this encoder on line breaks and trailing whitespace.
        let input = b"If you believe that truth=beauty, then surely mathematics is the most beautiful branch of philosophy.";
        let encoded = qp_encode(input);
        let expected = "If you believe that truth=3Dbeauty, then surely mathematics is the most bea=\r\nutiful branch of philosophy.";
        assert_eq!(encoded, expected);
        // First encoded line is exactly 76 chars including the
        // soft-break `=` at column 76 (75 payload + 1 marker). Pin
        // it structurally so a future off-by-one tweak gets caught.
        let lines: Vec<&str> = encoded.split("\r\n").collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 76);
        assert!(lines[0].ends_with('='));
        assert_qp_line_cap(&encoded);
        // Round-trip preserves the original byte sequence exactly.
        assert_eq!(qp_decode(encoded.as_bytes()).unwrap(), input);
    }

    #[test]
    fn qp_encode_empty_no_crlf() {
        // Empty input → empty output. No trailing CRLF, no soft
        // break. Pin so a future "always terminate with CRLF" tweak
        // doesn't slip through.
        assert_eq!(qp_encode(b""), "");
    }

    #[test]
    fn qp_encode_77_letters_wraps_at_75() {
        // 75 'a' chars fit on the first line as payload, then the
        // 76th 'a' would push line_len past the 75-char payload cap
        // — soft break first, emit on new line. Python's `quopri` writes
        // the same for this input.
        let input = vec![b'a'; 77];
        let encoded = qp_encode(&input);
        // Expected layout: 75 'a' + "=" (column 76) + "\r\n" + 2 'a'.
        let lines: Vec<&str> = encoded.split("\r\n").collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 76);
        assert_eq!(&lines[0][..75], "a".repeat(75));
        assert!(lines[0].ends_with('='));
        assert_eq!(lines[1], "aa");
        assert_qp_line_cap(&encoded);
    }

    #[test]
    fn qp_encode_high_byte_at_column_boundary() {
        // 73 'a' chars then a high byte. line_len=73, unit_len=3
        // for `=FF`. 73+3=76 ≤ 76 BUT > 75 → soft break first.
        // The triple is atomic (RFC §6.7 Rule 1) so it cannot be
        // split across the soft break.
        let mut input = vec![b'a'; 73];
        input.push(0xff);
        let encoded = qp_encode(&input);
        let lines: Vec<&str> = encoded.split("\r\n").collect();
        assert_eq!(lines.len(), 2);
        // First line: 73 'a' + soft-break `=` = 74 chars.
        assert_eq!(lines[0].len(), 74);
        assert!(lines[0].ends_with('='));
        // Second line: `=FF` (3 chars).
        assert_eq!(lines[1], "=FF");
        assert_qp_line_cap(&encoded);
    }

    #[test]
    fn qp_encode_matches_upstream_around_line_breaks() {
        // Each measured against upstream mimeTools through the plugin
        // harness. An LF is written with no soft break before it and
        // starts the next line at column 1, so that line holds 74
        // characters; a CR is an ordinary character.
        let a = |n: usize| "a".repeat(n);
        for (input, expected) in [
            (
                format!("{}\r\n{}", a(75), a(75)),
                format!("{}=\r\n\r\n{}=\r\na", a(75), a(74)),
            ),
            (format!("{}\n", a(76)), format!("{}=\r\na\n", a(75))),
            (format!("{}\n", a(75)), format!("{}\n", a(75))),
            (format!("{} \r\nb", a(74)), format!("{} =\r\n\r\nb", a(74))),
            (
                format!("{}=\r\nb", a(73)),
                format!("{}=\r\n=3D\r\nb", a(73)),
            ),
            (
                format!("{}\r{}", a(75), a(5)),
                format!("{}=\r\n\r{}", a(75), a(5)),
            ),
            (
                format!("{}\r\n{}", a(74), a(5)),
                format!("{}\r\n{}", a(74), a(5)),
            ),
        ] {
            let encoded = qp_encode(input.as_bytes());
            assert_eq!(encoded, expected, "{input:?}");
            assert_qp_line_cap(&encoded);
            assert_eq!(qp_decode(encoded.as_bytes()).unwrap(), input.as_bytes());
        }
    }

    #[test]
    fn qp_encode_space_at_wrap_boundary_is_literal() {
        // 75 'a' + SPACE + 'b': the space does not fit, so it goes after
        // the soft break, as it is.
        let mut input = vec![b'a'; 75];
        input.extend_from_slice(b" b");
        let encoded = qp_encode(&input);
        assert_eq!(encoded, format!("{}=\r\n b", "a".repeat(75)));
        assert_qp_line_cap(&encoded);
    }

    #[test]
    fn qp_encode_keeps_line_breaks() {
        // Text mode: line breaks are written as they are, as upstream
        // writes them. Until Phase 5 this encoder wrote `=0D=0A` for a
        // CRLF (binary mode).
        assert_eq!(qp_encode(b"abc\ndef"), "abc\ndef");
        assert_eq!(qp_encode(b"abc\r\ndef"), "abc\r\ndef");
        assert_eq!(qp_encode(b"line1\nline2\nline3"), "line1\nline2\nline3");
    }

    #[test]
    fn qp_round_trip_multiline() {
        // The decoder writes line breaks as they come and drops soft
        // breaks, so multi-line text round-trips byte for byte, long
        // lines and trailing spaces included.
        let long = format!(
            "{}\r\n{} \n\t{}",
            "x".repeat(200),
            "y".repeat(150),
            "z".repeat(80)
        );
        let inputs: &[&[u8]] = &[
            b"line1\nline2",
            b"line1\r\nline2\r\nline3",
            b"a\nb\nc",
            b"\n",
            b"\r\n\r\n",
            b"\r\r\n",
            b"trailing \r\n",
            long.as_bytes(),
        ];
        for input in inputs {
            let encoded = qp_encode(input);
            let decoded = qp_decode(encoded.as_bytes()).unwrap();
            assert_eq!(&decoded, input, "round-trip failed for {input:?}");
            assert_qp_line_cap(&encoded);
        }
    }

    #[test]
    fn qp_decode_round_trip_basic() {
        let inputs: &[&[u8]] = &[b"Hello!", b"a = b", &[0xc3, 0xa9], b"plain text"];
        for input in inputs {
            let encoded = qp_encode(input);
            let decoded = qp_decode(encoded.as_bytes()).unwrap();
            assert_eq!(&decoded, input);
            assert_qp_line_cap(&encoded);
        }
    }

    #[test]
    fn qp_round_trip_200_char_ascii() {
        // Exercise the multi-soft-break path: 200 ASCII chars wrap
        // into 3 encoded lines (75 + 75 + 50, with two soft breaks).
        // Each line must respect the 76-cap; round-trip must be
        // byte-exact.
        let input: Vec<u8> = (0..200).map(|i| b'a' + (i as u8 % 26)).collect();
        let encoded = qp_encode(&input);
        let decoded = qp_decode(encoded.as_bytes()).unwrap();
        assert_eq!(decoded, input);
        assert_qp_line_cap(&encoded);
        // Pin the line count — three lines means two soft breaks.
        let lines: Vec<&str> = encoded.split("\r\n").collect();
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn qp_round_trip_random_binary() {
        // Arbitrary byte sequences, with spaces, tabs, line breaks and
        // high bytes at varied positions. The deterministic LCG input
        // matters less than the line-cap + round-trip invariants
        // holding across the input space.
        for seed in 0..20u32 {
            let mut x = seed.wrapping_add(1).wrapping_mul(2_654_435_761);
            let len = 50 + (seed as usize % 250);
            let input: Vec<u8> = (0..len)
                .map(|_| {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    (x >> 16) as u8
                })
                .collect();
            let encoded = qp_encode(&input);
            let decoded = qp_decode(encoded.as_bytes()).unwrap();
            assert_eq!(decoded, input, "round-trip failed for seed {seed}");
            assert_qp_line_cap(&encoded);
        }
    }

    #[test]
    fn qp_decode_rejects_truncated_escape() {
        assert!(qp_decode(b"=").is_err());
        assert!(qp_decode(b"=3").is_err());
    }

    #[test]
    fn qp_decode_rejects_invalid_hex() {
        assert!(qp_decode(b"=ZZ").is_err());
        assert!(qp_decode(b"=3Z").is_err());
    }

    // ---- SAML --------------------------------------------------------------

    #[test]
    fn saml_decode_post_binding_xml_passthrough() {
        // POST binding: input is plain Base64 of the XML. URL-decode
        // is a no-op (no `%XX`), Base64-decode produces the XML, and
        // inflate fails — the function returns the Base64-decoded
        // bytes unchanged.
        let xml = b"<saml:Assertion>hello</saml:Assertion>";
        let b64 = base64_encode(xml, true);
        let decoded = saml_decode(b64.as_bytes()).unwrap();
        assert_eq!(decoded, xml);
    }

    #[test]
    fn saml_decode_redirect_binding_round_trip() {
        // Redirect binding: XML → DEFLATE → Base64 → URL encode.
        // The decoder should peel all three layers off.
        use miniz_oxide::deflate::compress_to_vec;
        let xml = b"<saml:LogoutRequest>x</saml:LogoutRequest>";
        let deflated = compress_to_vec(xml, 6);
        let b64 = base64_encode(&deflated, true);
        let url_encoded = url_encode_extended(b64.as_bytes());
        let decoded = saml_decode(url_encoded.as_bytes()).unwrap();
        assert_eq!(decoded, xml);
    }

    #[test]
    fn saml_decode_empty_passes_through() {
        // Empty input: URL-decode → "", Base64-decode → "", inflate
        // fails → return "". Should not panic.
        let decoded = saml_decode(b"").unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn saml_decode_propagates_invalid_base64() {
        // URL-decodes fine, but the base64 layer is malformed — the
        // error propagates up.
        assert!(saml_decode(b"not!base64").is_err());
    }

    #[test]
    fn saml_decode_rejects_decompression_bomb() {
        // Build a tiny DEFLATE stream that inflates well past the
        // 16 MiB cap — a zero-filled buffer compresses to a trivially
        // small payload. The decoder must REFUSE rather than allocate
        // 17+ MiB on attacker-controlled input (which on alloc
        // failure aborts the editor process and loses every tab's
        // unsaved work, since plugins are in-process per DESIGN.md
        // §6.5).
        use miniz_oxide::deflate::compress_to_vec;
        let bomb_input = vec![0u8; SAML_INFLATE_CAP + 1];
        let deflated = compress_to_vec(&bomb_input, 9);
        let b64 = base64_encode(&deflated, true);
        let url = url_encode_extended(b64.as_bytes());
        let result = saml_decode(url.as_bytes());
        assert!(
            result.is_err(),
            "decompression bomb must be refused, got Ok({:?} bytes)",
            result.as_ref().ok().map(std::vec::Vec::len),
        );
        // Diagnostic message must mention the bomb so the user
        // understands why their selection wasn't replaced.
        let msg = result.unwrap_err();
        assert!(
            msg.contains("bomb") || msg.contains("cap"),
            "diagnostic should reference the cap or the bomb threat, got: {msg}",
        );
    }
}
