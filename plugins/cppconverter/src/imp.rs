//! Entry-point implementations for cppconverter.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! The two menu items operate on the active Scintilla view's
//! selection: ASCII → HEX rewrites the selection as space-separated
//! uppercase hex bytes ("AB CD EF"); HEX → ASCII does the inverse.
//! Both are byte-level — Scintilla's buffer is UTF-8 but the hex
//! form is the standard hex-dump representation, which doesn't care
//! about encoding.

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

/// `WM_USER + 1000` — the NPPM_* base. Inlined to avoid a runtime
/// dependency on the host crate's constants from inside FFI bodies.
const NPPMSG: u32 = 0x0400 + 1000;
const NPPM_GETCURRENTSCINTILLA: u32 = NPPMSG + 4;
const NPPM_SETSTATUSBAR: u32 = NPPMSG + 24;
const STATUSBAR_DOC_TYPE: usize = 0;

const SCI_GETSELTEXT: u32 = 2161;
const SCI_GETSELECTIONSTART: u32 = 2143;
const SCI_GETSELECTIONEND: u32 = 2145;
const SCI_SETTARGETRANGE: u32 = 2686;
const SCI_REPLACETARGET: u32 = 2194;

/// Snapshot of the three handles `setInfo` delivers. Atomics so the
/// menu callback (UI thread) reads consistent values without taking
/// a lock — same pattern as `example-hello`.
static NPP_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static SCINTILLA_MAIN: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static SCINTILLA_SECONDARY: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

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

static FUNCS: SyncCell<[FuncItem; 2]> = SyncCell::new([
    FuncItem {
        item_name: menu_label(b"ASCII -> HEX"),
        p_func: Some(cmd_ascii_to_hex),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"HEX -> ASCII"),
        p_func: Some(cmd_hex_to_ascii),
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

/// Resolve the active Scintilla view's HWND. Asks the host
/// (NPPM_GETCURRENTSCINTILLA writes 0=main / 1=secondary into the
/// out-pointer) and looks up the matching handle stored by setInfo.
/// Returns null if setInfo hasn't run yet — the menu callbacks treat
/// null as "give up silently", which is what N++ plugins do too.
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

/// Read the active selection as raw bytes via `SCI_GETSELTEXT`.
///
/// Two-phase: first call with null `text` gets the length; second
/// call with a sized buffer fills it. This is Scintilla's documented
/// pattern for "get a string of a-priori-unknown length". Scintilla 5
/// returns the byte count of the selection (without the NUL); we
/// allocate `len + 1` so Scintilla can write its own terminator into
/// the trailing byte, then truncate it off before returning.
///
/// Uses `checked_add` + `try_from` on the `len + 1` allocation so a
/// pathological `isize::MAX` return from a future Scintilla on a
/// 32-bit target can't underflow into an undersized buffer (defense
/// in depth — Code++ targets x86_64 today, but the cost is one
/// branch).
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

/// Replace the active selection with `bytes`. Uses the
/// `SCI_SETTARGETRANGE` then `SCI_REPLACETARGET` pair so the
/// replacement is binary-safe: `SCI_REPLACETARGET` takes an explicit
/// length, whereas `SCI_REPLACESEL` reads its `lparam` as a
/// NUL-terminated C string and would silently truncate at the first
/// interior `\0`. Matters here because `hex_to_ascii` can produce
/// NULs from input like `"00 41 42"`.
fn replace_selection(sci: Hwnd, bytes: &[u8]) {
    if sci.is_null() {
        return;
    }
    // SAFETY: both SendMessages are pure queries with no pointer
    // arguments — wparam/lparam are zero. Scintilla's documented
    // return is the document-byte position.
    let (start, end) = unsafe {
        (
            SendMessageW(sci, SCI_GETSELECTIONSTART, 0, 0),
            SendMessageW(sci, SCI_GETSELECTIONEND, 0, 0),
        )
    };
    // SAFETY: SCI_SETTARGETRANGE takes wparam=start, lparam=end as
    // document positions. No pointer arguments. The caller has just
    // read both from Scintilla; passing them back is well-defined.
    unsafe {
        SendMessageW(sci, SCI_SETTARGETRANGE, start as usize, end);
    }
    // SAFETY: SCI_REPLACETARGET reads `wparam` bytes from `lparam`.
    // `bytes.as_ptr()` is valid for `bytes.len()` bytes for the
    // duration of the call; Scintilla doesn't retain the pointer
    // past the call. Length passed unmodified.
    unsafe {
        SendMessageW(sci, SCI_REPLACETARGET, bytes.len(), bytes.as_ptr() as isize);
    }
}

/// Set the host status bar's "doc-type" pane (slot 0) to the given
/// ASCII text. Plugins can drive this independently of the host's
/// own lang / encoding labels — typically used for transient feedback.
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

extern "C" fn cmd_ascii_to_hex() {
    let sci = active_scintilla();
    let bytes = get_selection_bytes(sci);
    if bytes.is_empty() {
        set_status("Converter: no selection");
        return;
    }
    let out = ascii_to_hex(&bytes);
    replace_selection(sci, out.as_bytes());
}

extern "C" fn cmd_hex_to_ascii() {
    let sci = active_scintilla();
    let bytes = get_selection_bytes(sci);
    if bytes.is_empty() {
        set_status("Converter: no selection");
        return;
    }
    match hex_to_ascii(&bytes) {
        Ok(out) => replace_selection(sci, &out),
        Err(msg) => set_status(&format!("Converter: {msg}")),
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
