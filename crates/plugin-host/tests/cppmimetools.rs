//! Integration test: load the in-tree `cppmimetools.dll`, assert the
//! lifecycle messages fire in order, and verify cmd-id assignment for
//! all twenty `FuncItem` entries (seventeen commands + three
//! separators — the host renders any `FuncItem` with `p_func: None`
//! as `MF_SEPARATOR`, and `PluginHost::lookup_cmd` correctly returns
//! `None` for the separator cmd-ids since their `p_func` is `None`).
//!
//! The pinned label order matches `plugins/cppmimetools/src/imp.rs`'s
//! `FUNCS` initialiser exactly — a re-order here changes muscle
//! memory for every user.
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

/// Positions of separator entries in the `FuncItem` array — 0-based
/// indices. Each is encoded as `FuncItem { p_func: None, .. }` in
/// `plugins/cppmimetools/src/imp.rs`; the host renders these as
/// `MF_SEPARATOR` in the rendered menu.
const SEPARATOR_POSITIONS: &[usize] = &[7, 10, 18];

/// Expected menu-item label order — matches `imp.rs`'s `FUNCS`
/// initialiser verbatim. Separator slots carry the sentinel `"---"`
/// label that the plugin writes (the host ignores it). A re-order
/// here would change every user's muscle memory, so pin it.
const EXPECTED_LABELS: &[&str] = &[
    "Base64 Encode",
    "Base64 Encode with padding",
    "Base64 Encode with Unix EOL",
    "Base64 Encode by line",
    "Base64 Decode",
    "Base64 Decode strict",
    "Base64 Decode by line",
    "---",
    "Quoted-printable Encode",
    "Quoted-printable Decode",
    "---",
    "URL Encode (RFC1738)",
    "URL Encode (RFC1738) by line",
    "URL Encode (Extended)",
    "URL Encode (Extended) by line",
    "URL Encode (Full)",
    "URL Encode (Full) by line",
    "URL Decode",
    "---",
    "SAML Decode",
];

#[test]
fn cppmimetools_loads_and_publishes_twenty_func_items() {
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
    assert_eq!(
        funcs.len(),
        EXPECTED_LABELS.len(),
        "cppmimetools contributes 17 commands + 3 separators = 20 FuncItem entries",
    );

    // Cmd-ids are sequential across both commands and separators —
    // the host walks every FuncItem slot and assigns ids in order.
    // Pin them all to catch any host-side regression that skips a
    // slot.
    for (i, item) in funcs.iter().enumerate() {
        assert_eq!(
            item.cmd_id,
            PLUGIN_CMD_ID_BASE + i as i32,
            "cmd-id mismatch at slot {i}",
        );
        let is_sep = SEPARATOR_POSITIONS.contains(&i);
        if is_sep {
            assert!(
                item.p_func.is_none(),
                "separator slot {i} should have p_func: None",
            );
        } else {
            assert!(
                item.p_func.is_some(),
                "command slot {i} should have p_func set",
            );
        }
    }

    // Menu labels in the documented order. Separator slots carry the
    // sentinel `"---"` label the plugin writes; the host renders
    // `MF_SEPARATOR` for those slots regardless.
    let labels: Vec<String> = funcs.iter().map(|f| decode_label(&f.item_name)).collect();
    assert_eq!(labels, EXPECTED_LABELS);

    // Lookup by cmd-id finds each command's callback; separator
    // cmd-ids resolve to None because their `p_func` is None.
    for (i, _) in funcs.iter().enumerate() {
        let id = PLUGIN_CMD_ID_BASE + i as i32;
        let resolved = host.lookup_cmd(id);
        if SEPARATOR_POSITIONS.contains(&i) {
            assert!(
                resolved.is_none(),
                "separator slot {i}'s cmd-id should not resolve to a callback",
            );
        } else {
            assert!(
                resolved.is_some(),
                "command slot {i}'s cmd-id should resolve to a callback",
            );
        }
    }
    // One past the last assigned id — should miss.
    assert!(host
        .lookup_cmd(PLUGIN_CMD_ID_BASE + EXPECTED_LABELS.len() as i32)
        .is_none());
}
