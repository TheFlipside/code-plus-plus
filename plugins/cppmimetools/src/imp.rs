//! Entry-point implementations for cppmimetools.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! Six menu items, three encode/decode pairs:
//! Base64 (RFC 4648), URL (RFC 3986 unreserved set), and
//! Quoted-Printable (RFC 2045 §6.7). Each operates on the active
//! Scintilla view's selection. Encode outputs are pure ASCII; decode
//! outputs are arbitrary bytes (could be any byte sequence the user's
//! input happens to decode to), so the selection round-trip uses the
//! binary-safe `SCI_SETTARGETRANGE` then `SCI_REPLACETARGET` pair.

#![cfg(target_os = "windows")]

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};

use codepp_plugin_host::ffi::{FuncItem, NppData, SCNotification, MENU_TITLE_LENGTH};

type Hwnd = *mut c_void;

#[link(name = "user32")]
extern "system" {
    fn SendMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
}

const NPPMSG: u32 = 0x0400 + 1000;
const NPPM_GETCURRENTSCINTILLA: u32 = NPPMSG + 4;
const NPPM_SETSTATUSBAR: u32 = NPPMSG + 24;
const STATUSBAR_DOC_TYPE: usize = 0;

const SCI_GETSELTEXT: u32 = 2161;
const SCI_GETSELECTIONSTART: u32 = 2143;
const SCI_GETSELECTIONEND: u32 = 2145;
const SCI_SETTARGETRANGE: u32 = 2686;
const SCI_REPLACETARGET: u32 = 2194;

static NPP_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static SCINTILLA_MAIN: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static SCINTILLA_SECONDARY: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

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

const fn menu_label(bytes: &[u8]) -> [u16; MENU_TITLE_LENGTH] {
    let mut buf = [0u16; MENU_TITLE_LENGTH];
    let mut i = 0;
    while i < bytes.len() && i < MENU_TITLE_LENGTH - 1 {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

#[repr(transparent)]
struct SyncCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for SyncCell<T> {}
impl<T> SyncCell<T> {
    const fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }
    fn get(&self) -> *mut T {
        self.0.get()
    }
}

static FUNCS: SyncCell<[FuncItem; 6]> = SyncCell::new([
    FuncItem {
        item_name: menu_label(b"Base64 Encode"),
        p_func: Some(cmd_base64_encode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"Base64 Decode"),
        p_func: Some(cmd_base64_decode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"URL Encode"),
        p_func: Some(cmd_url_encode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"URL Decode"),
        p_func: Some(cmd_url_decode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"Quoted-Printable Encode"),
        p_func: Some(cmd_qp_encode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"Quoted-Printable Decode"),
        p_func: Some(cmd_qp_decode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
]);

#[no_mangle]
pub extern "C" fn setInfo(data: NppData) {
    NPP_HANDLE.store(data.npp_handle, Ordering::Release);
    SCINTILLA_MAIN.store(data.scintilla_main_handle, Ordering::Release);
    SCINTILLA_SECONDARY.store(data.scintilla_second_handle, Ordering::Release);
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
        unsafe { *nb = 6 };
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

// ---- Scintilla helpers (same shape as cppconverter) ----

fn active_scintilla() -> Hwnd {
    let npp = NPP_HANDLE.load(Ordering::Acquire);
    if npp.is_null() {
        return core::ptr::null_mut();
    }
    let mut which: i32 = 0;
    // SAFETY: `&mut which` is a valid `int*` for the SendMessage
    // call. NPPM_GETCURRENTSCINTILLA is documented to write through
    // it (the host's dispatcher implements that contract).
    unsafe {
        SendMessageW(
            npp,
            NPPM_GETCURRENTSCINTILLA,
            0,
            &mut which as *mut i32 as isize,
        );
    }
    if which == 0 {
        SCINTILLA_MAIN.load(Ordering::Acquire)
    } else {
        SCINTILLA_SECONDARY.load(Ordering::Acquire)
    }
}

fn get_selection_bytes(sci: Hwnd) -> Vec<u8> {
    if sci.is_null() {
        return Vec::new();
    }
    // SAFETY: passing wparam=0, lparam=0 to SCI_GETSELTEXT asks for
    // the length only and writes nothing through any pointer.
    let len = unsafe { SendMessageW(sci, SCI_GETSELTEXT, 0, 0) };
    if len <= 0 {
        return Vec::new();
    }
    let len_us = match usize::try_from(len) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    let alloc = match len_us.checked_add(1) {
        Some(n) => n,
        None => return Vec::new(),
    };
    let mut buf = vec![0u8; alloc];
    // SAFETY: `buf.as_mut_ptr()` is valid for `len + 1` bytes;
    // Scintilla writes exactly that many through it for SCI_GETSELTEXT.
    unsafe {
        SendMessageW(sci, SCI_GETSELTEXT, 0, buf.as_mut_ptr() as isize);
    }
    buf.truncate(len_us);
    buf
}

fn replace_selection(sci: Hwnd, bytes: &[u8]) {
    if sci.is_null() {
        return;
    }
    // SAFETY: pure queries, no pointer arguments.
    let (start, end) = unsafe {
        (
            SendMessageW(sci, SCI_GETSELECTIONSTART, 0, 0),
            SendMessageW(sci, SCI_GETSELECTIONEND, 0, 0),
        )
    };
    // SAFETY: SCI_SETTARGETRANGE takes wparam=start, lparam=end as
    // document positions. No pointer arguments.
    unsafe {
        SendMessageW(sci, SCI_SETTARGETRANGE, start as usize, end);
    }
    // SAFETY: SCI_REPLACETARGET reads `wparam` bytes from `lparam`.
    // `bytes.as_ptr()` is valid for `bytes.len()` bytes for the call;
    // Scintilla doesn't retain the pointer.
    unsafe {
        SendMessageW(sci, SCI_REPLACETARGET, bytes.len(), bytes.as_ptr() as isize);
    }
}

fn set_status(text: &str) {
    let npp = NPP_HANDLE.load(Ordering::Acquire);
    if npp.is_null() {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().chain(core::iter::once(0)).collect();
    // SAFETY: `wide.as_ptr()` is a valid NUL-terminated UTF-16 buffer
    // for the duration of the call. NPPM_SETSTATUSBAR's `lparam` is
    // documented to take a `wchar_t*`.
    unsafe {
        SendMessageW(
            npp,
            NPPM_SETSTATUSBAR,
            STATUSBAR_DOC_TYPE,
            wide.as_ptr() as isize,
        );
    }
}

// ---- Menu callbacks ----

extern "C" fn cmd_base64_encode() {
    let sci = active_scintilla();
    let bytes = get_selection_bytes(sci);
    if bytes.is_empty() {
        set_status("MIME Tools: no selection");
        return;
    }
    let out = base64_encode(&bytes);
    replace_selection(sci, out.as_bytes());
}

extern "C" fn cmd_base64_decode() {
    let sci = active_scintilla();
    let bytes = get_selection_bytes(sci);
    if bytes.is_empty() {
        set_status("MIME Tools: no selection");
        return;
    }
    match base64_decode(&bytes) {
        Ok(out) => replace_selection(sci, &out),
        Err(msg) => set_status(&format!("MIME Tools: {msg}")),
    }
}

extern "C" fn cmd_url_encode() {
    let sci = active_scintilla();
    let bytes = get_selection_bytes(sci);
    if bytes.is_empty() {
        set_status("MIME Tools: no selection");
        return;
    }
    let out = url_encode(&bytes);
    replace_selection(sci, out.as_bytes());
}

extern "C" fn cmd_url_decode() {
    let sci = active_scintilla();
    let bytes = get_selection_bytes(sci);
    if bytes.is_empty() {
        set_status("MIME Tools: no selection");
        return;
    }
    match url_decode(&bytes) {
        Ok(out) => replace_selection(sci, &out),
        Err(msg) => set_status(&format!("MIME Tools: {msg}")),
    }
}

extern "C" fn cmd_qp_encode() {
    let sci = active_scintilla();
    let bytes = get_selection_bytes(sci);
    if bytes.is_empty() {
        set_status("MIME Tools: no selection");
        return;
    }
    let out = qp_encode(&bytes);
    replace_selection(sci, out.as_bytes());
}

extern "C" fn cmd_qp_decode() {
    let sci = active_scintilla();
    let bytes = get_selection_bytes(sci);
    if bytes.is_empty() {
        set_status("MIME Tools: no selection");
        return;
    }
    match qp_decode(&bytes) {
        Ok(out) => replace_selection(sci, &out),
        Err(msg) => set_status(&format!("MIME Tools: {msg}")),
    }
}

// ---- Base64 (RFC 4648, standard alphabet, `=` padding) ----

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    // Output is ceil(len / 3) * 4 ASCII chars.
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for c in &mut chunks {
        let v = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        s.push(B64_ALPHABET[((v >> 18) & 0x3f) as usize] as char);
        s.push(B64_ALPHABET[((v >> 12) & 0x3f) as usize] as char);
        s.push(B64_ALPHABET[((v >> 6) & 0x3f) as usize] as char);
        s.push(B64_ALPHABET[(v & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let v = (rem[0] as u32) << 16;
            s.push(B64_ALPHABET[((v >> 18) & 0x3f) as usize] as char);
            s.push(B64_ALPHABET[((v >> 12) & 0x3f) as usize] as char);
            s.push('=');
            s.push('=');
        }
        2 => {
            let v = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            s.push(B64_ALPHABET[((v >> 18) & 0x3f) as usize] as char);
            s.push(B64_ALPHABET[((v >> 12) & 0x3f) as usize] as char);
            s.push(B64_ALPHABET[((v >> 6) & 0x3f) as usize] as char);
            s.push('=');
        }
        _ => unreachable!("chunks_exact remainder length is < 3"),
    }
    s
}

fn base64_decode(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    // Strip whitespace; everything else must be a base64 char or `=`.
    let stripped: Vec<u8> = bytes
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if stripped.is_empty() {
        return Ok(Vec::new());
    }
    if !stripped.len().is_multiple_of(4) {
        return Err("base64 length is not a multiple of 4 (after whitespace strip)");
    }
    let mut out = Vec::with_capacity(stripped.len() / 4 * 3);
    for (chunk_idx, chunk) in stripped.chunks_exact(4).enumerate() {
        let is_last = chunk_idx == stripped.len() / 4 - 1;
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
                v = (v << 6) | (d as u32);
            }
        }
        // Always 3 bytes available in `v`; emit 3 minus pad count.
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

// ---- URL encoding (RFC 3986 unreserved set) ----

fn url_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if is_url_unreserved(b) {
            s.push(char::from(b));
        } else {
            s.push('%');
            s.push(HEX_UPPER[(b >> 4) as usize] as char);
            s.push(HEX_UPPER[(b & 0x0f) as usize] as char);
        }
    }
    s
}

fn is_url_unreserved(b: u8) -> bool {
    // RFC 3986 §2.3 — `unreserved = ALPHA / DIGIT / "-" / "." / "_" / "~"`.
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn url_decode(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    // Pass `+` through unchanged: Code++ buffers commonly contain
    // legitimate `+` (think `a + b` source code) and form-encoding's
    // "+ means space" convention would corrupt that. RFC 3986 itself
    // doesn't translate `+`.
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

// ---- Quoted-Printable (RFC 2045 §6.7) ----
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
    // is "Literal representation". Rule 3 adds TAB and SPACE — they
    // are literally representable except at end of line, and we
    // don't emit soft-line-breaks here (in-buffer transform), so
    // end-of-line whitespace isn't a concern for our output.
    // LF/CR are *not* safe: an unencoded LF would be ambiguous
    // with hard line breaks the transport layer might normalize,
    // so we always encode them as `=0A` / `=0D`.
    matches!(b, b'\t' | b' ' | 33..=60 | 62..=126)
}

fn qp_decode(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            // Soft line break: `=` followed by CRLF or LF — drop
            // the entire 2- or 3-byte sequence. Order doesn't matter
            // here: the LF check tests `bytes[i+1] == '\n'`, and a
            // CRLF sequence has `bytes[i+1] == '\r'`, so the LF
            // branch can't accidentally consume the `\r` half of a
            // CRLF. Either order is correct; LF first is shorter.
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            if i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
                i += 3;
                continue;
            }
            // Hex escape: `=XX`.
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Base64 ----

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
    fn base64_decode_rfc4648_test_vectors() {
        assert_eq!(base64_decode(b"").unwrap(), b"");
        assert_eq!(base64_decode(b"Zg==").unwrap(), b"f");
        assert_eq!(base64_decode(b"Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode(b"Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode(b"Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode(b"Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(base64_decode(b"Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn base64_round_trip_random_lengths() {
        for len in 0..=64usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 13) as u8).collect();
            let encoded = base64_encode(&data);
            let decoded = base64_decode(encoded.as_bytes()).unwrap();
            assert_eq!(decoded, data, "round trip failed at length {len}");
        }
    }

    #[test]
    fn base64_decode_skips_whitespace() {
        // Base64 in the wild often has line wrapping. Decode tolerates
        // arbitrary whitespace between any chars.
        assert_eq!(base64_decode(b"Zm9v\nYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode(b"  Zm9v  YmFy  ").unwrap(), b"foobar");
        assert_eq!(base64_decode(b"Z\tm\r\n9 v Y m F y").unwrap(), b"foobar");
    }

    #[test]
    fn base64_decode_rejects_invalid_chars() {
        assert!(base64_decode(b"!!!!").is_err());
        assert!(base64_decode(b"Zm9*").is_err());
    }

    #[test]
    fn base64_decode_rejects_bad_length() {
        // After stripping whitespace, length must be a multiple of 4.
        assert!(base64_decode(b"Zg=").is_err());
        assert!(base64_decode(b"Zm9").is_err());
    }

    #[test]
    fn base64_decode_rejects_pad_in_wrong_places() {
        // Padding in positions 0 or 1 of a 4-tuple is malformed.
        assert!(base64_decode(b"==Zm").is_err());
        assert!(base64_decode(b"Z===").is_err());
        // Padding-then-data within the same 4-tuple is malformed.
        assert!(base64_decode(b"Z=g=").is_err());
        // Padding in a non-final 4-tuple is malformed.
        assert!(base64_decode(b"Zg==Zm9v").is_err());
    }

    #[test]
    fn base64_encodes_high_bytes() {
        // Pin the encoding for bytes outside the ASCII range — these
        // are what makes base64 useful for binary data.
        assert_eq!(base64_encode(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(base64_encode(&[0x00, 0x00, 0x00]), "AAAA");
    }

    // ---- URL ----

    #[test]
    fn url_encode_unreserved_is_passthrough() {
        assert_eq!(url_encode(b"abcXYZ123-._~"), "abcXYZ123-._~");
    }

    #[test]
    fn url_encode_space() {
        assert_eq!(url_encode(b"hello world"), "hello%20world");
    }

    #[test]
    fn url_encode_special() {
        // `/?&=#` and friends all need encoding.
        assert_eq!(url_encode(b"a/b?c=d&e"), "a%2Fb%3Fc%3Dd%26e");
    }

    #[test]
    fn url_encode_high_bytes() {
        // Each byte is encoded independently — `é` in UTF-8 is 0xC3 0xA9.
        assert_eq!(url_encode(&[0xc3, 0xa9]), "%C3%A9");
    }

    #[test]
    fn url_decode_round_trip() {
        let cases: &[&[u8]] = &[
            b"hello world",
            b"a/b?c=d&e",
            b"plain",
            &[0xc3, 0xa9, 0x20, 0x66, 0x6f, 0x6f],
        ];
        for case in cases {
            let encoded = url_encode(case);
            let decoded = url_decode(encoded.as_bytes()).unwrap();
            assert_eq!(&decoded, case);
        }
    }

    #[test]
    fn url_decode_accepts_lowercase_hex() {
        assert_eq!(url_decode(b"%c3%a9").unwrap(), vec![0xc3, 0xa9]);
    }

    #[test]
    fn url_decode_passes_plus_through() {
        // Code++ buffers commonly contain `+` in code; treating it
        // as "space" per form-encoding would corrupt the input.
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

    // ---- Quoted-Printable ----

    #[test]
    fn qp_encode_passthrough_printable() {
        assert_eq!(qp_encode(b"Hello!"), "Hello!");
    }

    #[test]
    fn qp_encode_equals_sign() {
        // `=` is the escape character, must always be encoded.
        assert_eq!(qp_encode(b"a = b"), "a =3D b");
    }

    #[test]
    fn qp_encode_high_bytes() {
        // `é` in UTF-8 is 0xC3 0xA9. QP encodes both as =XX.
        assert_eq!(qp_encode(&[0xc3, 0xa9]), "=C3=A9");
    }

    #[test]
    fn qp_encode_control_chars() {
        // Newlines need encoding so a multi-line round-trip survives
        // transport-layer normalization. CR also gets encoded for
        // the same reason.
        assert_eq!(qp_encode(b"\n"), "=0A");
        assert_eq!(qp_encode(b"\r"), "=0D");
    }

    #[test]
    fn qp_encode_tab_and_space_are_safe() {
        // RFC 2045 §6.7 rule 3 — TAB and SPACE are literally
        // representable. Pinning so a future tightening doesn't
        // break existing content's round-trip readability.
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
        // `=\n` is a soft line break — drop both bytes.
        assert_eq!(qp_decode(b"foo=\nbar").unwrap(), b"foobar");
    }

    #[test]
    fn qp_decode_soft_break_crlf() {
        // `=\r\n` is the canonical soft-break form.
        assert_eq!(qp_decode(b"foo=\r\nbar").unwrap(), b"foobar");
    }

    #[test]
    fn qp_round_trip_multiline() {
        // Newlines round-trip via `=0A` / `=0D` — distinct from the
        // `=\n` soft-break path (which `qp_decode` only honors when
        // the `=` is followed by a literal LF, not a hex digit).
        // Pinning this so a future tweak that "smart-decodes" `=0A`
        // back to a soft-break can't silently break multi-line content.
        let inputs: &[&[u8]] = &[b"line1\nline2", b"line1\r\nline2\r\nline3", b"a\nb\nc"];
        for input in inputs {
            let encoded = qp_encode(input);
            let decoded = qp_decode(encoded.as_bytes()).unwrap();
            assert_eq!(&decoded, input, "round-trip failed for {input:?}");
            // Sanity: encoded form contains no literal LF/CR — they
            // got escaped as =0A / =0D, leaving only printable ASCII.
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
}
