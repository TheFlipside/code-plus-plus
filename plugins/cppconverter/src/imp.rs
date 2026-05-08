//! Entry-point implementations for cppconverter.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! The two menu items operate on the active Scintilla view's
//! selection: ASCII → HEX rewrites the selection as space-
//! separated uppercase hex bytes ("AB CD EF"); HEX → ASCII does
//! the inverse. Both are byte-level — Scintilla's buffer is UTF-8
//! but the hex form is the standard hex-dump representation,
//! which doesn't care about encoding.
//!
//! All shared FFI scaffolding (handle storage, `SyncCell`, the
//! NPPM/SCI message constants, the selection round-trip helpers,
//! the status-bar helper) lives in `codepp-plugin-sdk`. This file
//! keeps only the cppconverter-specific bits: the plugin name,
//! the two-item `FUNCS` array, the menu callbacks, and the pure-
//! Rust ASCII↔HEX transforms with their unit tests.

#![cfg(target_os = "windows")]

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
    let out = ascii_to_hex(&bytes);
    sdk::replace_selection(sci, out.as_bytes());
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

/// Format a byte slice as space-separated uppercase hex pairs.
/// "Hi!" → "48 69 21".
fn ascii_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        // Uppercase per the convention; lowercase would also work.
        s.push(HEX_UPPER[(b >> 4) as usize] as char);
        s.push(HEX_UPPER[(b & 0x0f) as usize] as char);
    }
    s
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

    #[test]
    fn ascii_to_hex_empty() {
        assert_eq!(ascii_to_hex(b""), "");
    }

    #[test]
    fn ascii_to_hex_single() {
        assert_eq!(ascii_to_hex(b"A"), "41");
    }

    #[test]
    fn ascii_to_hex_multibyte() {
        assert_eq!(ascii_to_hex(b"Hi!"), "48 69 21");
    }

    #[test]
    fn ascii_to_hex_full_byte_range() {
        // All four nibble combinations: 0x00 / 0x0f / 0xf0 / 0xff.
        // Pin both halves to catch a swapped nibble bug.
        assert_eq!(ascii_to_hex(&[0x00, 0x0f, 0xf0, 0xff]), "00 0F F0 FF");
    }

    #[test]
    fn ascii_to_hex_uppercase() {
        // Uppercase is the chosen convention; pin it so a future
        // tweak to lowercase becomes a deliberate decision.
        assert_eq!(ascii_to_hex(&[0xab, 0xcd]), "AB CD");
    }

    #[test]
    fn hex_to_ascii_round_trip_uppercase() {
        let bytes = b"Code++";
        let hex = ascii_to_hex(bytes);
        assert_eq!(hex_to_ascii(hex.as_bytes()).unwrap(), bytes);
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
        let hex = ascii_to_hex(&all);
        assert_eq!(hex_to_ascii(hex.as_bytes()).unwrap(), all);
    }
}
