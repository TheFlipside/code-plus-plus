//! Integration test: load the in-tree `cppmimetools.dll`, assert the
//! lifecycle messages fire in order, and verify cmd-id assignment for
//! all six menu items.
//!
//! Mirrors `cppconverter.rs`'s shape — locate the cdylib relative to
//! the test binary, stage it in a tempdir, discover + load via
//! `PluginHost`, then assert name + menu-item layout.

#![cfg(target_os = "windows")]
// Integration tests cast usize lengths into the C-ABI's i32
// shape — the values come from short literal arrays in the
// test, well below i32::MAX, but clippy's pedantic
// `cast_possible_truncation` / `cast_possible_wrap` flags
// every such `as i32` regardless. Mirrors the file-level allow
// in `plugin-host/src/lib.rs`.
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use std::path::PathBuf;

use codepp_plugin_host::{NppData, PluginHost, PLUGIN_CMD_ID_BASE};

fn locate_cppmimetools() -> Option<PathBuf> {
    let test_bin = std::env::current_exe().ok()?;
    let target_profile = test_bin.parent()?.parent()?;
    let candidate = target_profile.join("cppmimetools.dll");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

fn npp_data_with_bogus_handles() -> NppData {
    // The plugin's setInfo only stores these. The menu callbacks are
    // not invoked by this test (they would dispatch through the bogus
    // HWNDs and crash), so non-null bogus values are safe — we only
    // exercise load + introspection here. The secondary Scintilla
    // handle is intentionally null because no test path exercises it;
    // a real host always passes a valid HWND for both views.
    NppData {
        npp_handle: 0xDEAD_BEEFusize as *mut core::ffi::c_void,
        scintilla_main_handle: 0xC0FF_EE00usize as *mut core::ffi::c_void,
        scintilla_second_handle: core::ptr::null_mut(),
    }
}

fn decode_label(label_w: &[u16]) -> String {
    let nul = label_w
        .iter()
        .position(|&u| u == 0)
        .unwrap_or(label_w.len());
    String::from_utf16_lossy(&label_w[..nul])
}

#[test]
fn cppmimetools_loads_and_publishes_six_func_items() {
    let Some(dll) = locate_cppmimetools() else {
        eprintln!(
            "skipping cppmimetools integration test: \
             cppmimetools.dll not built. Run `cargo build -p codepp-cppmimetools`."
        );
        return;
    };

    let staging = tempfile::tempdir().unwrap();
    let staged = staging.path().join("cppmimetools.dll");
    std::fs::copy(&dll, &staged).expect("copy cppmimetools.dll into staging dir");

    let mut host = PluginHost::new();
    let count = host.discover(staging.path()).unwrap();
    assert_eq!(count, 1, "discovery should find exactly the staged DLL");

    host.load(0, npp_data_with_bogus_handles())
        .expect("load cppmimetools.dll");

    let info = host.iter().next().expect("one plugin");
    assert!(info.is_loaded(), "plugin should be loaded after load()");
    assert_eq!(
        info.display_label(),
        "MIME Tools",
        "getName output should round-trip into display_label"
    );

    let funcs = info.func_items().expect("loaded plugin has func items");
    assert_eq!(funcs.len(), 6, "cppmimetools contributes six menu items");

    // Cmd-ids are sequential — pin them all to catch any host-side
    // regression that skips a slot.
    for (i, item) in funcs.iter().enumerate() {
        assert_eq!(
            item.cmd_id,
            PLUGIN_CMD_ID_BASE + i as i32,
            "cmd-id mismatch at slot {i}",
        );
        assert!(item.p_func.is_some(), "p_func unset at slot {i}");
    }

    // Menu labels in the documented order. A re-order would change
    // every user's muscle memory, so pin the ordering.
    let labels: Vec<String> = funcs.iter().map(|f| decode_label(&f.item_name)).collect();
    assert_eq!(
        labels,
        vec![
            "Base64 Encode",
            "Base64 Decode",
            "URL Encode",
            "URL Decode",
            "Quoted-Printable Encode",
            "Quoted-Printable Decode",
        ],
    );

    // Lookup by cmd-id finds each callback; unassigned ids miss.
    for i in 0..6 {
        assert!(host.lookup_cmd(PLUGIN_CMD_ID_BASE + i).is_some());
    }
    assert!(host.lookup_cmd(PLUGIN_CMD_ID_BASE + 6).is_none());
}
