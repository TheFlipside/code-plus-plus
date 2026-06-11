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
//! host's `populate_plugin_menu` renders any entry with no callback
//! as `MF_SEPARATOR` (see `ui_win32/src/lib.rs` near
//! `if func.p_func.is_none()`).
//!
//! Every command operates on the active Scintilla view's selection:
//! reads it as bytes, transforms it, and writes the result back. The
//! selection round-trip uses the binary-safe `SCI_SETTARGETRANGE` /
//! `SCI_REPLACETARGET` pair, so encode outputs that are pure ASCII
//! AND decode outputs that are arbitrary bytes both survive the
//! buffer write intact.

#![cfg(target_os = "windows")]

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
    run_encode(|b| base64_encode(b).into_bytes());
}

extern "C" fn cmd_base64_encode_with_padding() {
    run_encode(|b| base64_encode_wrapped(b, "\r\n").into_bytes());
}

extern "C" fn cmd_base64_encode_unix_eol() {
    run_encode(|b| base64_encode_wrapped(b, "\n").into_bytes());
}

extern "C" fn cmd_base64_encode_by_line() {
    run_encode(|b| for_each_line(b, |line| base64_encode(line).into_bytes()));
}

extern "C" fn cmd_base64_decode() {
    run_decode(base64_decode_tolerant);
}

extern "C" fn cmd_base64_decode_strict() {
    run_decode(base64_decode_strict);
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
        let out = for_each_line(b, |line| match base64_decode_tolerant(line) {
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

// ---- Base64 (RFC 4648, standard alphabet, `=` padding) --------------------
//
// `base64_encode`: padded, no wrapping — compact form, suitable for
//                  inline data URIs and config files.
// `base64_encode_wrapped`: padded, wrapped at 76 columns with the
//                          supplied EOL bytes (`\r\n` for MIME, `\n`
//                          for the Unix variant).
// `base64_decode_tolerant`: skip any ASCII whitespace, accept input
//                            whose length (post-strip) is a multiple
//                            of 4. Rejects invalid chars and malformed
//                            padding. Matches RFC 4648 §3.3 (tolerant
//                            implementations MAY ignore whitespace).
// `base64_decode_strict`: reject ANY byte not in the base64 alphabet,
//                          `=`, or one of the four whitespace bytes
//                          that RFC 4648 carves out as "non-essential"
//                          — actually, strict means even those are
//                          rejected. We reject any byte not in
//                          `A-Z a-z 0-9 + / =`.

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_WRAP_WIDTH: usize = 76;

fn base64_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for c in &mut chunks {
        let v = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        s.push(B64_ALPHABET[((v >> 18) & 0x3f) as usize] as char);
        s.push(B64_ALPHABET[((v >> 12) & 0x3f) as usize] as char);
        s.push(B64_ALPHABET[((v >> 6) & 0x3f) as usize] as char);
        s.push(B64_ALPHABET[(v & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let v = u32::from(rem[0]) << 16;
            s.push(B64_ALPHABET[((v >> 18) & 0x3f) as usize] as char);
            s.push(B64_ALPHABET[((v >> 12) & 0x3f) as usize] as char);
            s.push('=');
            s.push('=');
        }
        2 => {
            let v = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            s.push(B64_ALPHABET[((v >> 18) & 0x3f) as usize] as char);
            s.push(B64_ALPHABET[((v >> 12) & 0x3f) as usize] as char);
            s.push(B64_ALPHABET[((v >> 6) & 0x3f) as usize] as char);
            s.push('=');
        }
        _ => unreachable!("chunks_exact remainder length is < 3"),
    }
    s
}

fn base64_encode_wrapped(bytes: &[u8], eol: &str) -> String {
    // Standard base64, then insert `eol` every 76 output chars. The
    // final line is NOT terminated by `eol` — matches RFC 2045's
    // "no final CRLF on the last line of a 7-bit text body" rule and
    // keeps the output minimal for the selection-replace case (the
    // user can re-add a trailing newline themselves if they want one).
    let s = base64_encode(bytes);
    let mut out = String::with_capacity(s.len() + (s.len() / B64_WRAP_WIDTH) * eol.len());
    for (i, c) in s.chars().enumerate() {
        if i > 0 && i.is_multiple_of(B64_WRAP_WIDTH) {
            out.push_str(eol);
        }
        out.push(c);
    }
    out
}

fn base64_decode_tolerant(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let stripped: Vec<u8> = bytes
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    base64_decode_core(&stripped)
}

fn base64_decode_strict(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    // Strict mode: any whitespace or non-alphabet byte fails the
    // input. Validate the byte set first, then delegate to the same
    // core decoder so the two modes share the padding / length /
    // value logic.
    for &b in bytes {
        if !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')) {
            return Err("base64 strict: input contains non-alphabet byte");
        }
    }
    base64_decode_core(bytes)
}

fn base64_decode_core(stripped: &[u8]) -> Result<Vec<u8>, &'static str> {
    if stripped.is_empty() {
        return Ok(Vec::new());
    }
    if !stripped.len().is_multiple_of(4) {
        return Err("base64 length is not a multiple of 4 (after whitespace strip)");
    }
    let mut out = Vec::with_capacity(stripped.len() / 4 * 3);
    let last_chunk_idx = stripped.len() / 4 - 1;
    for (chunk_idx, chunk) in stripped.chunks_exact(4).enumerate() {
        let is_last = chunk_idx == last_chunk_idx;
        let mut v = 0u32;
        let mut pad = 0;
        for (i, &c) in chunk.iter().enumerate() {
            if c == b'=' {
                if !is_last {
                    return Err("base64 padding before end of input");
                }
                if i < 2 {
                    return Err("base64 padding in first two positions");
                }
                pad += 1;
                v <<= 6;
            } else {
                if pad > 0 {
                    return Err("base64 non-pad after padding");
                }
                let d = b64_digit(c).ok_or("invalid base64 character")?;
                v = (v << 6) | u32::from(d);
            }
        }
        out.push(((v >> 16) & 0xff) as u8);
        if pad < 2 {
            out.push(((v >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((v & 0xff) as u8);
        }
    }
    Ok(out)
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
// Encode emits no soft line breaks (`=\r\n`) — output isn't intended
// for transport over an 80-col-limited channel; it's intended for
// the user to read in the editor. Decode tolerates them anyway so
// pasted MIME content round-trips correctly.

fn qp_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if is_qp_safe(b) {
            s.push(char::from(b));
        } else {
            s.push('=');
            s.push(HEX_UPPER[(b >> 4) as usize] as char);
            s.push(HEX_UPPER[(b & 0x0f) as usize] as char);
        }
    }
    s
}

fn is_qp_safe(b: u8) -> bool {
    // RFC 2045 §6.7 rule 2: printable ASCII (33..=126) except `=`
    // is "Literal representation". Rule 3 adds TAB and SPACE.
    // LF/CR are NOT safe: an unencoded LF would be ambiguous with
    // hard line breaks the transport layer might normalize, so we
    // always encode them as `=0A` / `=0D`.
    matches!(b, b'\t' | b' ' | 33..=60 | 62..=126)
}

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
//   2. Base64 decode (tolerant). Strips any line wrapping the SAML
//      tooling may have inserted.
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
    let b64_decoded = base64_decode_tolerant(&url_decoded)?;
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
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_encode_rfc4648_test_vectors() {
        // The canonical test vectors from RFC 4648 §10.
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encode_wrapped_crlf_at_76() {
        // 60 bytes of input → 80 base64 chars → exactly one wrap at
        // col 76. The wrap inserts CRLF; the final 4 chars don't get
        // a trailing CRLF (matches RFC 2045's "no final newline" rule).
        let input = vec![b'A'; 60];
        let encoded = base64_encode_wrapped(&input, "\r\n");
        // `A` × 60 → `Q` × 60 (base64 of 'A' triples is "QUFB"-ish but
        // with the actual bit pattern). Just check the structural
        // properties: exactly one CRLF, first line is 76 chars.
        let lines: Vec<&str> = encoded.split("\r\n").collect();
        assert_eq!(lines.len(), 2, "should be exactly two lines");
        assert_eq!(lines[0].len(), 76);
        assert_eq!(lines[1].len(), 80 - 76); // remaining 4 chars
    }

    #[test]
    fn base64_encode_wrapped_lf_at_76() {
        // Unix EOL variant — same structure as CRLF but with `\n`.
        let input = vec![b'A'; 60];
        let encoded = base64_encode_wrapped(&input, "\n");
        let lines: Vec<&str> = encoded.split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 76);
    }

    #[test]
    fn base64_encode_wrapped_short_no_wrap() {
        // Input shorter than 76 base64 chars — no wrap, single line.
        let encoded = base64_encode_wrapped(b"hello", "\r\n");
        assert_eq!(encoded, "aGVsbG8=");
        assert!(!encoded.contains('\n'));
    }

    #[test]
    fn base64_decode_rfc4648_test_vectors() {
        assert_eq!(base64_decode_tolerant(b"").unwrap(), b"");
        assert_eq!(base64_decode_tolerant(b"Zg==").unwrap(), b"f");
        assert_eq!(base64_decode_tolerant(b"Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode_tolerant(b"Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode_tolerant(b"Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode_tolerant(b"Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(base64_decode_tolerant(b"Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn base64_round_trip_random_lengths() {
        for len in 0..=64usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 13) as u8).collect();
            let encoded = base64_encode(&data);
            let decoded = base64_decode_tolerant(encoded.as_bytes()).unwrap();
            assert_eq!(decoded, data, "round trip failed at length {len}");
        }
    }

    #[test]
    fn base64_decode_tolerant_skips_whitespace() {
        // Tolerant mode tolerates arbitrary whitespace between chars.
        assert_eq!(base64_decode_tolerant(b"Zm9v\nYmFy").unwrap(), b"foobar");
        assert_eq!(
            base64_decode_tolerant(b"  Zm9v  YmFy  ").unwrap(),
            b"foobar"
        );
        assert_eq!(
            base64_decode_tolerant(b"Z\tm\r\n9 v Y m F y").unwrap(),
            b"foobar"
        );
    }

    #[test]
    fn base64_decode_strict_rejects_whitespace() {
        // Strict mode: ANY non-alphabet byte fails. Whitespace counts.
        assert!(base64_decode_strict(b"Zm9v\nYmFy").is_err());
        assert!(base64_decode_strict(b" Zm9vYmFy").is_err());
        // But pure base64 still decodes cleanly.
        assert_eq!(base64_decode_strict(b"Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode_strict(b"Zg==").unwrap(), b"f");
    }

    #[test]
    fn base64_decode_rejects_invalid_chars() {
        assert!(base64_decode_tolerant(b"!!!!").is_err());
        assert!(base64_decode_tolerant(b"Zm9*").is_err());
    }

    #[test]
    fn base64_decode_rejects_bad_length() {
        // After stripping whitespace, length must be a multiple of 4.
        assert!(base64_decode_tolerant(b"Zg=").is_err());
        assert!(base64_decode_tolerant(b"Zm9").is_err());
    }

    #[test]
    fn base64_decode_rejects_pad_in_wrong_places() {
        assert!(base64_decode_tolerant(b"==Zm").is_err());
        assert!(base64_decode_tolerant(b"Z===").is_err());
        assert!(base64_decode_tolerant(b"Z=g=").is_err());
        assert!(base64_decode_tolerant(b"Zg==Zm9v").is_err());
    }

    #[test]
    fn base64_encodes_high_bytes() {
        assert_eq!(base64_encode(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(base64_encode(&[0x00, 0x00, 0x00]), "AAAA");
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
        let encoded = for_each_line(input, |line| base64_encode(line).into_bytes());
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
        assert_eq!(qp_encode(b"\n"), "=0A");
        assert_eq!(qp_encode(b"\r"), "=0D");
    }

    #[test]
    fn qp_encode_tab_and_space_are_safe() {
        assert_eq!(qp_encode(b" "), " ");
        assert_eq!(qp_encode(b"\t"), "\t");
        assert_eq!(qp_encode(b"a b\tc"), "a b\tc");
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

    #[test]
    fn qp_round_trip_multiline() {
        let inputs: &[&[u8]] = &[b"line1\nline2", b"line1\r\nline2\r\nline3", b"a\nb\nc"];
        for input in inputs {
            let encoded = qp_encode(input);
            let decoded = qp_decode(encoded.as_bytes()).unwrap();
            assert_eq!(&decoded, input, "round-trip failed for {input:?}");
            assert!(
                !encoded.bytes().any(|b| b == b'\n' || b == b'\r'),
                "encoded form should have no literal CR/LF: {encoded:?}",
            );
        }
    }

    #[test]
    fn qp_decode_round_trip_basic() {
        let inputs: &[&[u8]] = &[b"Hello!", b"a = b", &[0xc3, 0xa9], b"plain text"];
        for input in inputs {
            let encoded = qp_encode(input);
            let decoded = qp_decode(encoded.as_bytes()).unwrap();
            assert_eq!(&decoded, input);
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
        let b64 = base64_encode(xml);
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
        let b64 = base64_encode(&deflated);
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
        let b64 = base64_encode(&deflated);
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
