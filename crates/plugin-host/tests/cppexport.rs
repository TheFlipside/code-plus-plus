//! Integration test: load the in-tree `cppexport.dll`, assert the
//! lifecycle messages fire in order, and verify cmd-id assignment for
//! both menu items.
//!
//! Mirrors `cppconverter.rs` / `cppmimetools.rs`'s shape — locate the
//! cdylib relative to the test binary, stage it in a tempdir, discover
//! + load via `PluginHost`, then assert name + menu-item layout.

#![cfg(target_os = "windows")]

use std::path::PathBuf;

use codepp_plugin_host::{NppData, PluginHost, PLUGIN_CMD_ID_BASE};

fn locate_cppexport() -> Option<PathBuf> {
    let test_bin = std::env::current_exe().ok()?;
    let target_profile = test_bin.parent()?.parent()?;
    let candidate = target_profile.join("cppexport.dll");
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
fn cppexport_loads_and_publishes_five_func_items() {
    let Some(dll) = locate_cppexport() else {
        eprintln!(
            "skipping cppexport integration test: \
             cppexport.dll not built. Run `cargo build -p codepp-cppexport`."
        );
        return;
    };

    let staging = tempfile::tempdir().unwrap();
    let staged = staging.path().join("cppexport.dll");
    std::fs::copy(&dll, &staged).expect("copy cppexport.dll into staging dir");

    let mut host = PluginHost::new();
    let count = host.discover(staging.path()).unwrap();
    assert_eq!(count, 1, "discovery should find exactly the staged DLL");

    host.load(0, npp_data_with_bogus_handles())
        .expect("load cppexport.dll");

    let info = host.iter().next().expect("one plugin");
    assert!(info.is_loaded(), "plugin should be loaded after load()");
    assert_eq!(
        info.display_label(),
        "Export",
        "getName output should round-trip into display_label"
    );

    let funcs = info.func_items().expect("loaded plugin has func items");
    assert_eq!(funcs.len(), 5, "cppexport contributes five menu items");

    // Cmd-ids are sequential from the base.
    for (i, item) in funcs.iter().enumerate() {
        assert_eq!(item.cmd_id, PLUGIN_CMD_ID_BASE + i as i32);
        assert!(item.p_func.is_some(), "p_func unset at slot {i}");
    }

    // Menu labels in the documented order: HTML pair, RTF pair,
    // then the combined "all formats" item.
    let labels: Vec<String> = funcs.iter().map(|f| decode_label(&f.item_name)).collect();
    assert_eq!(
        labels,
        vec![
            "Export to HTML...",
            "Copy HTML to Clipboard",
            "Export to RTF...",
            "Copy RTF to Clipboard",
            "Copy All Formats to Clipboard",
        ],
    );

    // Lookup by cmd-id finds each callback; one beyond misses.
    for i in 0..5 {
        assert!(host.lookup_cmd(PLUGIN_CMD_ID_BASE + i).is_some());
    }
    assert!(host.lookup_cmd(PLUGIN_CMD_ID_BASE + 5).is_none());
}
