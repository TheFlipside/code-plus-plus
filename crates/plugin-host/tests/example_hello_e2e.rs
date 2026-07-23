//! End-to-end load-and-dispatch test against the real, compiled
//! `example-hello` plugin `.so`.
//!
//! This exercises the whole non-Windows plugin pipeline against an
//! actual cdylib, without a GUI: `dlopen`, resolving the six entry
//! points plus `codepp_plugin_set_dispatch`, the `setInfo` handshake,
//! `getFuncsArray`, and — the part that only works once the SDK's
//! transport and the host's routing are correct — invoking the plugin's
//! menu command and observing that its `SendMessage` calls
//! (`NPPM_GETCURRENTSCINTILLA` then `SCI_INSERTTEXT`) reach the host.
//!
//! A recording mock stands in for the GTK routing function and the real
//! Scintilla widget: it answers `NPPM_GETCURRENTSCINTILLA` and records
//! the `SCI_INSERTTEXT` payload, so the assertion is exact ("Hello from
//! plugin") rather than a screenshot.
//!
//! `#[ignore]` because it needs `libexample_hello.so`, which
//! `cargo build --workspace` produces but `cargo test -p
//! codepp-plugin-host` does not. Run after a workspace build:
//! `cargo test -p codepp-plugin-host --test example_hello_e2e -- --ignored`.

#![cfg(target_os = "linux")]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;

use codepp_plugin_host::{NppData, PluginHost, NPPMSG};

/// `NPPM_GETCURRENTSCINTILLA` — writes the active view index (0/1)
/// through its `lparam` `int*` and returns 0.
const NPPM_GETCURRENTSCINTILLA: u32 = NPPMSG + 4;
/// `SCI_INSERTTEXT(pos, *utf8)`.
const SCI_INSERTTEXT: u32 = 2003;

// Sentinels the mock recognises. Their *addresses* are the handles the
// plugin sees in `NppData` and routes messages back to.
static NPP_SENTINEL: u8 = 0;
static SCI_SENTINEL: u8 = 0;

/// Everything the mock dispatch recorded, so the test can assert on it.
static RECORDED: Mutex<Recorded> = Mutex::new(Recorded {
    got_getcurrentscintilla: false,
    inserted: None,
});

struct Recorded {
    got_getcurrentscintilla: bool,
    inserted: Option<String>,
}

fn npp_ptr() -> *mut c_void {
    std::ptr::addr_of!(NPP_SENTINEL).cast_mut().cast()
}
fn sci_ptr() -> *mut c_void {
    std::ptr::addr_of!(SCI_SENTINEL).cast_mut().cast()
}

/// The stand-in for the GTK routing function: routes by handle identity,
/// answering `NPPM_GETCURRENTSCINTILLA` (view 0) and recording the text
/// of any `SCI_INSERTTEXT` sent to the Scintilla sentinel.
extern "C" fn mock_dispatch(hwnd: *mut c_void, msg: u32, _wparam: usize, lparam: isize) -> isize {
    if std::ptr::eq(hwnd, npp_ptr()) {
        if msg == NPPM_GETCURRENTSCINTILLA {
            // Write the active view index (0 = main) through the plugin's
            // `int*` out-pointer, exactly as `dispatch_nppm` does.
            if lparam != 0 {
                // SAFETY: the plugin passed a valid `&mut i32`.
                unsafe {
                    *(lparam as *mut i32) = 0;
                }
            }
            RECORDED.lock().unwrap().got_getcurrentscintilla = true;
        }
        return 0;
    }
    if std::ptr::eq(hwnd, sci_ptr()) && msg == SCI_INSERTTEXT && lparam != 0 {
        // SAFETY: `SCI_INSERTTEXT`'s lparam is a NUL-terminated UTF-8
        // string the plugin keeps alive across the call.
        let text = unsafe { std::ffi::CStr::from_ptr(lparam as *const std::os::raw::c_char) }
            .to_string_lossy()
            .into_owned();
        RECORDED.lock().unwrap().inserted = Some(text);
    }
    0
}

/// Locate `libexample_hello.so` under the workspace `target/<profile>/`.
fn built_plugin() -> Option<PathBuf> {
    // CARGO_MANIFEST_DIR = .../crates/plugin-host; the workspace target
    // dir is two levels up. Try both debug and release profiles.
    let ws = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .to_path_buf();
    for profile in ["debug", "release"] {
        let p = ws.join("target").join(profile).join("libexample_hello.so");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[test]
#[ignore = "needs `cargo build --workspace` to produce libexample_hello.so first"]
fn example_hello_inserts_via_the_dispatch_pipeline() {
    let Some(so) = built_plugin() else {
        eprintln!("skipping: libexample_hello.so not built (run `cargo build --workspace`)");
        return;
    };

    // Stage into the `<dir>/<stem>.so` layout discovery requires.
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("example_hello");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::copy(&so, plugin_dir.join("example_hello.so")).unwrap();

    let mut host = PluginHost::new();
    let n = host.discover(tmp.path()).unwrap();
    assert_eq!(n, 1, "should discover exactly the one staged plugin");

    let npp_data = NppData {
        npp_handle: npp_ptr(),
        scintilla_main_handle: sci_ptr(),
        scintilla_second_handle: std::ptr::null_mut(),
    };
    // Load with the routing callback installed (the SDK handshake).
    host.load(0, npp_data, Some(mock_dispatch))
        .expect("example-hello should load");

    // getName / getFuncsArray ran during load; the one menu command is
    // now registered with a host-assigned cmd id.
    let cmd_id = host
        .iter()
        .next()
        .and_then(|p| p.func_items())
        .and_then(|f| f.first())
        .map(|f| f.cmd_id)
        .expect("example-hello should contribute one FuncItem");

    let cmd = host.lookup_cmd(cmd_id).expect("cmd id should resolve");
    // Invoke the plugin's "Insert Hello" callback. It calls
    // `active_scintilla()` (→ NPPM_GETCURRENTSCINTILLA) then
    // `SendMessageW(sci, SCI_INSERTTEXT, …)`, both of which route through
    // `mock_dispatch` via the SDK transport we installed above.
    // SAFETY: a plugin `FuncItem.p_func`, invoked with no arguments per
    // the ABI, on this (single) thread.
    unsafe { cmd() };

    let recorded = RECORDED.lock().unwrap();
    assert!(
        recorded.got_getcurrentscintilla,
        "plugin should have queried NPPM_GETCURRENTSCINTILLA"
    );
    assert_eq!(
        recorded.inserted.as_deref(),
        Some("Hello from plugin"),
        "plugin should have inserted its text via SCI_INSERTTEXT"
    );
}
