//! Entry-point implementations for cppconverter.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! The two menu items operate on the active Scintilla view's
//! selection: ASCII → HEX rewrites the selection as uppercase hex
//! bytes with no separator, 16 bytes to a line ("ABCDEF"), as the
//! upstream `NppConverter` does with its default settings; HEX → ASCII
//! does the inverse, and takes the pairs with or without whitespace
//! between them. Both are byte-level — Scintilla's buffer is UTF-8
//! but the hex form doesn't care about encoding. Upstream reads its
//! format from a `converter.ini` (a separator, lower case, the line
//! length); Code++'s plugin has no settings and always writes the
//! defaults.
//!
//! All shared FFI scaffolding (handle storage, `SyncCell`, the
//! NPPM/SCI message constants, the selection round-trip helpers,
//! the status-bar helper) lives in `codepp-plugin-sdk`. This file
//! keeps only the cppconverter-specific bits: the plugin name,
//! the two-item `FUNCS` array, the menu callbacks, and the pure-
//! Rust ASCII↔HEX transforms with their unit tests.

#![cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]

use codepp_plugin_sdk::{self as sdk, FuncItem, NppData, SCNotification, SyncCell};

const PLUGIN_NAME: [u16; 10] = make_plugin_name();

const fn make_plugin_name() -> [u16; 10] {
    // "Converter\0" — 9 ASCII chars + NUL.
    let mut buf = [0u16; 10];
    let bytes = b"Converter";
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

static FUNCS: SyncCell<[FuncItem; 2]> = SyncCell::new([
    FuncItem {
        item_name: sdk::menu_label(b"ASCII -> HEX"),
        p_func: Some(cmd_ascii_to_hex),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"HEX -> ASCII"),
        p_func: Some(cmd_hex_to_ascii),
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
        unsafe { *nb = 2 };
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

extern "C" fn cmd_ascii_to_hex() {
    let sci = sdk::active_scintilla();
    let bytes = sdk::get_selection_bytes(sci);
    if bytes.is_empty() {
        sdk::set_status("Converter: no selection");
        return;
    }
    let out = ascii_to_hex(&bytes, sdk::document_eol(sci));
    sdk::replace_selection(sci, &out);
}

extern "C" fn cmd_hex_to_ascii() {
    let sci = sdk::active_scintilla();
    let bytes = sdk::get_selection_bytes(sci);
    if bytes.is_empty() {
        sdk::set_status("Converter: no selection");
        return;
    }
    match hex_to_ascii(&bytes) {
        Ok(out) => sdk::replace_selection(sci, &out),
        Err(msg) => sdk::set_status(&format!("Converter: {msg}")),
    }
}

/// Input bytes per line of "ASCII -> HEX" output: the upstream
/// `NppConverter`'s default (`nbCharPerLine=16` in its `converter.ini`).
const HEX_BYTES_PER_LINE: usize = 16;

/// Format a byte slice as uppercase hex pairs with no separator and
/// `eol` after every [`HEX_BYTES_PER_LINE`]th input byte, so a full
/// last line ends with one and a partial one does not: "Hi!" →
/// "486921". That is the upstream `NppConverter`'s output with its
/// default settings, measured against the one Notepad++ 8.9.8.1 ships.
/// A line break in the input is encoded like any other byte (`0D0A`).
fn ascii_to_hex(bytes: &[u8], eol: &[u8]) -> Vec<u8> {
    let full_lines = bytes.len() / HEX_BYTES_PER_LINE;
    let mut out = Vec::with_capacity(bytes.len() * 2 + full_lines * eol.len());
    for line in bytes.chunks(HEX_BYTES_PER_LINE) {
        for b in line {
            out.push(HEX_UPPER[(b >> 4) as usize]);
            out.push(HEX_UPPER[(b & 0x0f) as usize]);
        }
        if line.len() == HEX_BYTES_PER_LINE {
            out.extend_from_slice(eol);
        }
    }
    out
}

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

/// Parse `bytes` as space- and/or newline-separated pairs of hex
/// digits and return the decoded byte string. Whitespace between
/// pairs is ignored; whitespace inside a pair is rejected. Returns
/// `Err` with a short user-facing message on any parse problem so
/// the menu callback can surface it via the status bar without
/// touching the selection.
fn hex_to_ascii(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(bytes.len() / 3 + 1);
    let mut nibble: Option<u8> = None;
    for &c in bytes {
        if c.is_ascii_whitespace() {
            if nibble.is_some() {
                return Err("odd hex digit before whitespace");
            }
            continue;
        }
        let val = hex_digit(c).ok_or("invalid hex digit")?;
        match nibble.take() {
            None => nibble = Some(val),
            Some(hi) => out.push((hi << 4) | val),
        }
    }
    if nibble.is_some() {
        return Err("odd hex digit at end of input");
    }
    Ok(out)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ascii_to_hex` with a CRLF document, as text.
    fn hex(bytes: &[u8]) -> String {
        String::from_utf8(ascii_to_hex(bytes, b"\r\n")).unwrap()
    }

    #[test]
    fn ascii_to_hex_empty() {
        assert_eq!(hex(b""), "");
    }

    #[test]
    fn ascii_to_hex_single() {
        assert_eq!(hex(b"A"), "41");
    }

    #[test]
    fn ascii_to_hex_multibyte() {
        // Measured against the upstream NppConverter on "hello".
        assert_eq!(hex(b"hello"), "68656C6C6F");
        assert_eq!(hex(b"Hi!"), "486921");
    }

    #[test]
    fn ascii_to_hex_full_byte_range() {
        // All four nibble combinations: 0x00 / 0x0f / 0xf0 / 0xff.
        // Pin both halves to catch a swapped nibble bug.
        assert_eq!(hex(&[0x00, 0x0f, 0xf0, 0xff]), "000FF0FF");
    }

    #[test]
    fn ascii_to_hex_uppercase() {
        // Upstream's default; pin it so a change is deliberate.
        assert_eq!(hex(&[0xab, 0xcd]), "ABCD");
    }

    #[test]
    fn ascii_to_hex_encodes_line_breaks_as_bytes() {
        // Measured: upstream turns "ab\r\ncd" into "61620D0A6364".
        assert_eq!(hex(b"ab\r\ncd"), "61620D0A6364");
    }

    #[test]
    fn ascii_to_hex_breaks_the_line_every_16_bytes() {
        // Measured: every full line ends with a line break, the last
        // one included; a partial last line has none.
        let line = "41".repeat(16);
        assert_eq!(hex(&[b'A'; 15]), "41".repeat(15));
        assert_eq!(hex(&[b'A'; 16]), format!("{line}\r\n"));
        assert_eq!(hex(&[b'A'; 17]), format!("{line}\r\n41"));
        assert_eq!(hex(&[b'A'; 32]), format!("{line}\r\n{line}\r\n"));
        // The break is the document's own line ending.
        for eol in ["\n", "\r"] {
            let out = ascii_to_hex(&[b'A'; 17], eol.as_bytes());
            assert_eq!(String::from_utf8(out).unwrap(), format!("{line}{eol}41"));
        }
    }

    #[test]
    fn hex_to_ascii_round_trip_uppercase() {
        let bytes = b"Code++";
        assert_eq!(hex_to_ascii(hex(bytes).as_bytes()).unwrap(), bytes);
        // Across line breaks too.
        let long: Vec<u8> = (0u8..40).collect();
        assert_eq!(hex_to_ascii(hex(&long).as_bytes()).unwrap(), long);
    }

    #[test]
    fn hex_to_ascii_accepts_lowercase() {
        // Symmetric: encode is uppercase-only, decode accepts both
        // so a user-typed "ab cd" doesn't fail.
        assert_eq!(hex_to_ascii(b"ab cd").unwrap(), vec![0xab, 0xcd]);
    }

    #[test]
    fn hex_to_ascii_no_separators() {
        // Standalone hex string without spaces is fine — common
        // when the user pastes a hex blob from elsewhere.
        assert_eq!(hex_to_ascii(b"4869").unwrap(), b"Hi");
    }

    #[test]
    fn hex_to_ascii_newlines_are_whitespace() {
        assert_eq!(hex_to_ascii(b"48\n69\r\n21").unwrap(), b"Hi!");
    }

    #[test]
    fn hex_to_ascii_rejects_invalid_digit() {
        assert!(hex_to_ascii(b"ZZ").is_err());
    }

    #[test]
    fn hex_to_ascii_rejects_odd_length() {
        assert!(hex_to_ascii(b"ABC").is_err());
    }

    #[test]
    fn hex_to_ascii_rejects_split_byte() {
        // "A B" — the high nibble of a byte followed by whitespace
        // is malformed. Catching this means partial parses don't
        // produce truncated output.
        assert!(hex_to_ascii(b"A B").is_err());
    }

    #[test]
    fn hex_to_ascii_empty_input_is_ok() {
        // Empty input round-trips to empty output. Distinct from
        // a parse error; the menu callback's "no selection" branch
        // catches that earlier so this path isn't hit in practice,
        // but the function should still be total.
        assert_eq!(hex_to_ascii(b"").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_to_ascii_whitespace_only_is_ok() {
        // Whitespace-only collapses to empty output, not a parse
        // error. Pinning this so a future "reject if no digits seen"
        // tweak doesn't silently break a paste that happens to be
        // whitespace-only.
        assert_eq!(hex_to_ascii(b"   ").unwrap(), Vec::<u8>::new());
        assert_eq!(hex_to_ascii(b"\n\t  \r\n").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_to_ascii_full_byte_range() {
        let all: Vec<u8> = (0u8..=255).collect();
        assert_eq!(hex_to_ascii(hex(&all).as_bytes()).unwrap(), all);
    }
}
