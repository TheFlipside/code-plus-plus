//! End-to-end load-and-dispatch test against the real, compiled
//! `cppexport` plugin `.so`.
//!
//! This is the cross-platform payoff of the `cppexport` port: the plugin
//! no longer touches the OS clipboard or Save-As dialog directly, but
//! hands the host semantic bytes through the two Code++ extension
//! messages `CODEPPM_SETCLIPBOARD` / `CODEPPM_EXPORTSAVEDIALOG`. This
//! test drives the *actual* cdylib — `dlopen`, the six entry points, the
//! `codepp_plugin_set_dispatch` handshake, `getFuncsArray` — then invokes
//! its "Copy HTML" and "Export to HTML" menu commands and observes that
//! the rendered HTML reaches the host via those messages, with no GUI.
//!
//! A recording mock stands in for the GTK routing function *and* the
//! real Scintilla widget: it answers the buffer/style queries the export
//! path issues (`SCI_GETLENGTH`, `SCI_GETSTYLEDTEXTFULL`,
//! `SCI_STYLEGET*`) for a tiny two-character buffer, so the plugin
//! produces real HTML, and it records the `CODEPPM_*` payload so the
//! assertions are exact rather than a screenshot. It parses the request
//! structs itself (mirroring `dispatch_nppm`, which is separately
//! unit-tested) so the assertion is on what the *plugin* actually sent.
//!
//! `#[ignore]` because it needs `libcppexport.so`, which `cargo build
//! --workspace` produces but `cargo test -p codepp-plugin-host` does not.
//! Run after a workspace build:
//! `cargo test -p codepp-plugin-host --test cppexport_e2e -- --ignored`.

#![cfg(target_os = "linux")]
// The mock routes many message numbers to the same `0` ("unhandled")
// result; collapsing those arms would obscure which messages the export
// path actually issues, so keep them explicit.
#![allow(clippy::match_same_arms)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;

use codepp_plugin_host::{
    ClipboardSetRequest, ExportSaveRequest, NppData, PluginHost, CLIP_FORMAT_HTML,
    CODEPPM_EXPORTSAVEDIALOG, CODEPPM_SETCLIPBOARD, EXPORT_KIND_HTML, NPPMSG,
};

// --- Scintilla / NPPM message numbers the export path exercises ---
const NPPM_GETCURRENTSCINTILLA: u32 = NPPMSG + 4;
const SCI_GETLENGTH: u32 = 2006;
const SCI_GETSTYLEDTEXTFULL: u32 = 2778;
const SCI_STYLEGETFORE: u32 = 2481;
const SCI_STYLEGETBACK: u32 = 2482;
const SCI_STYLEGETBOLD: u32 = 2483;
const SCI_STYLEGETITALIC: u32 = 2484;
const SCI_STYLEGETSIZE: u32 = 2485;
const SCI_STYLEGETFONT: u32 = 2486;

/// The buffer the mock "editor" holds — two ASCII bytes, all style 0.
const BUFFER: &[u8] = b"hi";

/// Mirror of Scintilla's `Sci_TextRangeFull` (the `lparam` shape of
/// `SCI_GETSTYLEDTEXTFULL`), enough to read `cp_max` and the output
/// pointer the plugin allocated for us to fill.
#[repr(C)]
struct SciTextRangeFull {
    _cp_min: isize,
    cp_max: isize,
    lpstr_text: *mut u8,
}

// This is a second, independent mirror of the plugin's own
// `Sci_TextRangeFull` (`plugins/cppexport/src/imp.rs`) — a reorder or
// added field there would silently desync this copy rather than fail to
// compile. Pin the layout so such a drift trips here at build time:
// two `Sci_Position` (`isize`) + one pointer, no padding on any target
// where `isize` and a pointer share a width.
const _: () = assert!(
    core::mem::size_of::<SciTextRangeFull>()
        == 2 * core::mem::size_of::<isize>() + core::mem::size_of::<*mut u8>()
);

// Sentinels the mock recognises; their *addresses* are the handles the
// plugin sees in `NppData` and routes messages back to.
static NPP_SENTINEL: u8 = 0;
static SCI_SENTINEL: u8 = 0;

// One process-global recorder shared by both `#[test]`s. They run in
// parallel under cargo's default harness, so the isolation invariant is
// FIELD SEPARATION: each test touches only its own field
// (`clipboard` / `export`) and resets just that field before invoking
// its command. A future third test MUST claim a fresh field (or the
// suite must go single-threaded, as `ui_gtk/src/platform.rs` does for
// its shared-state tests) rather than reuse one — sharing a field would
// reintroduce a cross-test race.
static RECORDED: Mutex<Recorded> = Mutex::new(Recorded {
    clipboard: None,
    export: None,
});

struct Recorded {
    /// `(format, bytes)` of the single clipboard payload cppexport's
    /// "Copy HTML" command sends.
    clipboard: Option<(u32, Vec<u8>)>,
    /// `(bytes, suggested_name, kind)` of the export request "Export to
    /// HTML" sends.
    export: Option<(Vec<u8>, String, u32)>,
}

fn npp_ptr() -> *mut c_void {
    std::ptr::addr_of!(NPP_SENTINEL).cast_mut().cast()
}
fn sci_ptr() -> *mut c_void {
    std::ptr::addr_of!(SCI_SENTINEL).cast_mut().cast()
}

/// Decode a NUL-terminated wide (UTF-16) string the plugin passed.
unsafe fn wide_to_string(mut p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut units = Vec::new();
    // SAFETY: caller guarantees a NUL-terminated wide buffer.
    unsafe {
        while *p != 0 {
            units.push(*p);
            p = p.add(1);
        }
    }
    String::from_utf16_lossy(&units)
}

/// Stand-in for the GTK routing function. Routes by handle identity:
/// NPPM/CODEPPM messages to the "host", SCI_* to the "editor". Answers
/// the buffer/style queries the export path issues, and records the two
/// Code++ extension messages.
extern "C" fn mock_dispatch(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize {
    // Host side (NppData._nppHandle): NPPM_* + the CODEPPM_* extensions.
    if std::ptr::eq(hwnd, npp_ptr()) {
        match msg {
            NPPM_GETCURRENTSCINTILLA => {
                if lparam != 0 {
                    // SAFETY: the plugin passed a valid `&mut i32`.
                    unsafe { *(lparam as *mut i32) = 0 };
                }
                0
            }
            CODEPPM_SETCLIPBOARD => {
                if lparam == 0 {
                    return 0;
                }
                // SAFETY: cppexport sent a valid `ClipboardSetRequest`.
                let req = lparam as *const ClipboardSetRequest;
                let (count, entries) = unsafe { ((*req).count, (*req).entries) };
                // Record the first non-empty entry (the "Copy HTML"
                // command sends exactly one). `data` may be null when
                // `data_len == 0` per the ABI, and `from_raw_parts`
                // requires non-null even for a zero-length slice, so
                // guard both before dereferencing.
                if count > 0 && !entries.is_null() {
                    let e = unsafe { &*entries };
                    if !e.data.is_null() && e.data_len > 0 {
                        let bytes =
                            unsafe { std::slice::from_raw_parts(e.data, e.data_len) }.to_vec();
                        RECORDED
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clipboard = Some((e.format, bytes));
                    }
                }
                1
            }
            CODEPPM_EXPORTSAVEDIALOG => {
                if lparam == 0 {
                    return 0;
                }
                // SAFETY: cppexport sent a valid `ExportSaveRequest`.
                let req = lparam as *const ExportSaveRequest;
                let (data, data_len, name_ptr, kind) = unsafe {
                    (
                        (*req).data,
                        (*req).data_len,
                        (*req).suggested_name,
                        (*req).kind,
                    )
                };
                // `from_raw_parts` requires a non-null pointer even for
                // a zero-length slice; the export path never sends empty
                // data, but guard defensively.
                let bytes = if data.is_null() || data_len == 0 {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(data, data_len) }.to_vec()
                };
                let name = unsafe { wide_to_string(name_ptr) };
                RECORDED
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .export = Some((bytes, name, kind));
                1
            }
            _ => 0,
        }
    } else if std::ptr::eq(hwnd, sci_ptr()) {
        // Editor side: answer the export path's read queries for a
        // two-byte, single-style buffer.
        match msg {
            SCI_GETLENGTH => isize::try_from(BUFFER.len()).unwrap_or(0),
            SCI_GETSTYLEDTEXTFULL => {
                if lparam == 0 {
                    return 0;
                }
                // SAFETY: the plugin passed a valid `Sci_TextRangeFull`
                // whose `lpstr_text` is sized for `2*cp_max + 2` bytes.
                let range = lparam as *const SciTextRangeFull;
                let (cp_max, out) = unsafe { ((*range).cp_max, (*range).lpstr_text) };
                let n = usize::try_from(cp_max.max(0))
                    .unwrap_or(0)
                    .min(BUFFER.len());
                // Interleave (text_byte, style_byte=0) pairs + trailing
                // NUL pair, exactly as Scintilla does.
                for (i, &b) in BUFFER.iter().take(n).enumerate() {
                    unsafe {
                        *out.add(2 * i) = b;
                        *out.add(2 * i + 1) = 0;
                    }
                }
                unsafe {
                    *out.add(2 * n) = 0;
                    *out.add(2 * n + 1) = 0;
                }
                0
            }
            // Style 0 attributes: black on white, not bold/italic, 11pt.
            SCI_STYLEGETFORE => 0x0000_0000, // 0x00BBGGRR black
            SCI_STYLEGETBACK => 0x00FF_FFFF, // white
            SCI_STYLEGETBOLD | SCI_STYLEGETITALIC => 0,
            SCI_STYLEGETSIZE => 11,
            // Length phase returns 0 → "use default font" (empty name),
            // so no second (buffer-writing) call happens. `wparam` is the
            // style index; unused here.
            SCI_STYLEGETFONT => {
                let _ = wparam;
                0
            }
            _ => 0,
        }
    } else {
        0
    }
}

/// Locate `libcppexport.so` under the workspace `target/<profile>/`.
fn built_plugin() -> Option<PathBuf> {
    let ws = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .to_path_buf();
    for profile in ["debug", "release"] {
        let p = ws.join("target").join(profile).join("libcppexport.so");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Load the real `.so` and return the host holding it, staged into the
/// `<dir>/<stem>.so` layout discovery requires. Keeps the `tempdir`
/// alive by returning it alongside.
fn load_cppexport() -> Option<(PluginHost, tempfile::TempDir)> {
    let so = built_plugin()?;
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("cppexport");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::copy(&so, plugin_dir.join("cppexport.so")).unwrap();

    let mut host = PluginHost::new();
    let n = host.discover(tmp.path()).unwrap();
    assert_eq!(n, 1, "should discover exactly the one staged plugin");

    let npp_data = NppData {
        npp_handle: npp_ptr(),
        scintilla_main_handle: sci_ptr(),
        scintilla_second_handle: std::ptr::null_mut(),
    };
    host.load(0, npp_data, Some(mock_dispatch))
        .expect("cppexport should load");
    Some((host, tmp))
}

/// Invoke the `FuncItem` at `idx` in the (single) loaded plugin's array.
fn invoke_cmd(host: &PluginHost, idx: usize) {
    let cmd_id = host
        .iter()
        .next()
        .and_then(|p| p.func_items())
        .and_then(|f| f.get(idx))
        .map(|f| f.cmd_id)
        .expect("cppexport should contribute five FuncItems");
    let cmd = host.lookup_cmd(cmd_id).expect("cmd id should resolve");
    // SAFETY: a plugin `FuncItem.p_func`, invoked with no arguments per
    // the ABI, on this (single) thread.
    unsafe { cmd() };
}

#[test]
#[ignore = "needs `cargo build --workspace` to produce libcppexport.so first"]
fn cppexport_copy_html_sends_rendered_html_via_setclipboard() {
    let Some((host, _tmp)) = load_cppexport() else {
        eprintln!("skipping: libcppexport.so not built (run `cargo build --workspace`)");
        return;
    };
    RECORDED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clipboard = None;

    // FuncItem index 1 is "Copy HTML to Clipboard" (0 = Export to HTML).
    invoke_cmd(&host, 1);

    let rec = RECORDED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (format, bytes) = rec
        .clipboard
        .as_ref()
        .expect("Copy HTML should have sent a CODEPPM_SETCLIPBOARD payload");
    assert_eq!(*format, CLIP_FORMAT_HTML, "payload must be tagged HTML");
    let html = String::from_utf8_lossy(bytes);
    // Real rendered HTML: a document wrapping the buffer text.
    assert!(html.contains("<body>"), "expected an HTML body: {html:?}");
    assert!(html.contains("hi"), "expected the buffer text: {html:?}");
}

#[test]
#[ignore = "needs `cargo build --workspace` to produce libcppexport.so first"]
fn cppexport_export_html_sends_bytes_via_exportsavedialog() {
    let Some((host, _tmp)) = load_cppexport() else {
        eprintln!("skipping: libcppexport.so not built (run `cargo build --workspace`)");
        return;
    };
    RECORDED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .export = None;

    // FuncItem index 0 is "Export to HTML...".
    invoke_cmd(&host, 0);

    let rec = RECORDED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (bytes, name, kind) = rec
        .export
        .as_ref()
        .expect("Export to HTML should have sent a CODEPPM_EXPORTSAVEDIALOG request");
    assert_eq!(*kind, EXPORT_KIND_HTML, "export kind must be HTML");
    assert_eq!(name, "export.html", "suggested file name");
    let html = String::from_utf8_lossy(bytes);
    assert!(
        html.contains("<body>") && html.contains("hi"),
        "rendered HTML: {html:?}"
    );
}
