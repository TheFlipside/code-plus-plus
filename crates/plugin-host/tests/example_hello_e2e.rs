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
//! It also covers the plugin's guard against a host with no display.
//! `load_blocking` delivers `NPPN_TBMODIFICATION`, where example-hello
//! registers its dock panel, and this process never opens a display:
//! the plugin must build no widget there. Before the guard existed, GTK
//! aborted this test's process at the first one.
//!
//! `#[ignore]` because it needs `libexample_hello.so`, which
//! `cargo build --workspace` produces but `cargo test -p
//! codepp-plugin-host` does not. Run after a workspace build:
//! `cargo test -p codepp-plugin-host --test example_hello_e2e -- --ignored`.

#![cfg(target_os = "linux")]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;

use codepp_plugin_host::{notify_all, Notification, NppData, PluginHost, NPPMSG};

/// `NPPM_GETCURRENTSCINTILLA` — writes the active view index (0/1)
/// through its `lparam` `int*` and returns 0.
const NPPM_GETCURRENTSCINTILLA: u32 = NPPMSG + 4;
/// `NPPM_SETSTATUSBAR(section, *wchar)`.
const NPPM_SETSTATUSBAR: u32 = NPPMSG + 24;
/// `NPPM_GETFULLPATHFROMBUFFERID(id, *wchar)` — writes the path and
/// returns the unit count including the NUL, or -1 for an unknown id.
const NPPM_GETFULLPATHFROMBUFFERID: u32 = NPPMSG + 58;
/// `SCI_INSERTTEXT(pos, *utf8)`.
const SCI_INSERTTEXT: u32 = 2003;

/// The buffer id the mock host "knows", and the path it answers with.
const CLOSING_ID: usize = 7;
const CLOSING_PATH: &str = "/tmp/closing.txt";

// Sentinels the mock recognises. Their *addresses* are the handles the
// plugin sees in `NppData` and routes messages back to.
static NPP_SENTINEL: u8 = 0;
static SCI_SENTINEL: u8 = 0;

/// Everything the mock dispatch recorded, so the test can assert on it.
/// Shared by both tests in this binary, which run in parallel: each
/// test asserts only on the fields its own plugin call writes
/// (`got_getcurrentscintilla` / `inserted` for the command,
/// `path_queried_for` / `status` for the notification), so keep any
/// new test's fields disjoint too.
static RECORDED: Mutex<Recorded> = Mutex::new(Recorded {
    got_getcurrentscintilla: false,
    inserted: None,
    path_queried_for: None,
    status: None,
});

struct Recorded {
    got_getcurrentscintilla: bool,
    inserted: Option<String>,
    /// The buffer id of the last `NPPM_GETFULLPATHFROMBUFFERID`.
    path_queried_for: Option<usize>,
    /// The text of the last `NPPM_SETSTATUSBAR`.
    status: Option<String>,
}

/// Decode a NUL-terminated wide string a plugin passed as `lparam`.
///
/// # Safety
///
/// `ptr` must point at a NUL-terminated UTF-16 buffer that stays live
/// for the call.
unsafe fn wide_to_string(ptr: *const u16) -> String {
    let mut len = 0usize;
    // SAFETY: caller's contract — reads stop at the NUL.
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were just read through `ptr`.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
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
extern "C" fn mock_dispatch(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize {
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
        if msg == NPPM_GETFULLPATHFROMBUFFERID {
            RECORDED.lock().unwrap().path_queried_for = Some(wparam);
            if wparam != CLOSING_ID {
                return -1;
            }
            if lparam == 0 {
                return 260;
            }
            let wide: Vec<u16> = CLOSING_PATH.encode_utf16().chain(Some(0)).collect();
            assert!(
                wide.len() <= 260,
                "the mock must respect the MAX_PATH contract"
            );
            // SAFETY: the plugin passed a MAX_PATH-unit wide buffer, per
            // the message's contract; the path is far shorter (asserted).
            unsafe {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), lparam as *mut u16, wide.len());
            }
            return wide.len().cast_signed();
        }
        if msg == NPPM_SETSTATUSBAR && lparam != 0 {
            // SAFETY: `NPPM_SETSTATUSBAR`'s lparam is a NUL-terminated
            // wide string the plugin keeps alive across the call.
            let text = unsafe { wide_to_string(lparam as *const u16) };
            RECORDED.lock().unwrap().status = Some(text);
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
    host.load_blocking(0, npp_data, Some(mock_dispatch))
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

/// The other half of the pipeline, in the other direction: the host
/// delivers `NPPN_FILEBEFORECLOSE` and the plugin, **from inside its
/// `beNotified`**, calls back with `NPPM_GETFULLPATHFROMBUFFERID` to
/// learn which file is closing, then reports it on the status bar.
///
/// This is the round trip DESIGN.md §7.4's synchronous-notification
/// item exists for. What it pins here is the plugin's side — that the
/// callback is made from the notification and the answer is used —
/// through the same SDK transport the host installs. The host's side
/// (delivering before the tab is removed, and answering a re-entrant
/// `NPPM_*` from `beNotified`) is pinned by `codepp-shell`'s
/// `announced_close_*` tests and each backend's source-scan guards.
#[test]
#[ignore = "needs `cargo build --workspace` to produce libexample_hello.so first"]
fn example_hello_resolves_the_closing_path_from_inside_file_before_close() {
    let Some(so) = built_plugin() else {
        eprintln!("skipping: libexample_hello.so not built (run `cargo build --workspace`)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("example_hello");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::copy(&so, plugin_dir.join("example_hello.so")).unwrap();

    let mut host = PluginHost::new();
    assert_eq!(host.discover(tmp.path()).unwrap(), 1);
    let npp_data = NppData {
        npp_handle: npp_ptr(),
        scintilla_main_handle: sci_ptr(),
        scintilla_second_handle: std::ptr::null_mut(),
    };
    host.load_blocking(0, npp_data, Some(mock_dispatch))
        .expect("example-hello should load");

    notify_all(
        &host,
        &Notification::FileBeforeClose {
            buffer_id: CLOSING_ID.cast_signed(),
        },
        npp_ptr(),
    );

    let recorded = RECORDED.lock().unwrap();
    assert_eq!(
        recorded.path_queried_for,
        Some(CLOSING_ID),
        "the plugin should resolve the closing buffer's path from inside beNotified"
    );
    assert_eq!(
        recorded.status.as_deref(),
        Some("Closing: /tmp/closing.txt"),
        "the plugin should report the resolved path on the status bar"
    );
}
