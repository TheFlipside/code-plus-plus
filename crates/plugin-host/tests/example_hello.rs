//! Integration test: load the in-tree `example_hello.dll`, assert the
//! lifecycle messages fire in order, and verify cmd-id assignment.
//!
//! The plugin is a workspace cdylib; cargo emits its DLL into
//! `<workspace>/target/<profile>/example_hello.dll` when the workspace
//! is built. We locate it relative to the test binary's own path —
//! `target/<profile>/deps/<this_test>.exe` — by walking up two
//! directory levels and probing for `example_hello.dll`.
//!
//! If the DLL is absent (e.g. `cargo test -p codepp-plugin-host` was
//! run before `cargo build -p codepp-example-hello`), the test prints
//! a skip message rather than failing. CI runs `cargo build
//! --workspace` before tests, so the DLL is present in CI.

#![cfg(target_os = "windows")]

use std::path::PathBuf;

use codepp_plugin_host::{NppData, PluginHost, PLUGIN_CMD_ID_BASE};

fn locate_example_hello() -> Option<PathBuf> {
    let test_bin = std::env::current_exe().ok()?;
    // target/<profile>/deps/<this_test>.exe → target/<profile>/example_hello.dll
    let target_profile = test_bin.parent()?.parent()?;
    let candidate = target_profile.join("example_hello.dll");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

fn npp_data_with_bogus_handles() -> NppData {
    // The plugin's setInfo only stores these; getFuncsArray and
    // getName don't dereference them. Non-null bogus values are
    // safe — example-hello checks for null in its menu callback,
    // but the menu callback is not invoked by this test.
    NppData {
        npp_handle: 0xDEAD_BEEFusize as *mut core::ffi::c_void,
        scintilla_main_handle: 0xC0FF_EE00usize as *mut core::ffi::c_void,
        scintilla_second_handle: core::ptr::null_mut(),
    }
}

#[test]
fn example_hello_loads_and_publishes_one_func_item() {
    let Some(dll) = locate_example_hello() else {
        eprintln!(
            "skipping example_hello integration test: \
             example_hello.dll not built. Run `cargo build -p codepp-example-hello`."
        );
        return;
    };

    // Stage the DLL in an isolated tempdir so PluginHost::discover
    // doesn't sweep up unrelated artifacts that share `target/debug`.
    let staging = tempfile::tempdir().unwrap();
    let staged = staging.path().join("example_hello.dll");
    std::fs::copy(&dll, &staged).expect("copy example_hello.dll into staging dir");

    let mut host = PluginHost::new();
    let count = host.discover(staging.path()).unwrap();
    assert_eq!(count, 1, "discovery should find exactly the staged DLL");

    host.load(0, npp_data_with_bogus_handles())
        .expect("load example_hello.dll");

    let info = host.iter().next().expect("one plugin");
    assert!(info.is_loaded(), "plugin should be loaded after load()");
    assert_eq!(
        info.display_label(),
        "Example Hello",
        "getName output should round-trip into display_label"
    );

    let funcs = info.func_items().expect("loaded plugin has func items");
    assert_eq!(funcs.len(), 1, "example-hello contributes one menu item");
    assert_eq!(
        funcs[0].cmd_id, PLUGIN_CMD_ID_BASE,
        "first plugin loaded gets the cmd-id base"
    );

    // The plugin's func ptr is non-null (defined as
    // `Some(plugin_cmd_insert_hello)` in the static FuncItem).
    assert!(
        funcs[0].p_func.is_some(),
        "p_func should be the menu callback"
    );

    // Lookup by cmd-id finds the same callback.
    assert!(
        host.lookup_cmd(PLUGIN_CMD_ID_BASE).is_some(),
        "lookup_cmd should resolve the assigned cmd-id"
    );
    assert!(
        host.lookup_cmd(PLUGIN_CMD_ID_BASE - 1).is_none(),
        "lookup_cmd should miss for unassigned ids"
    );

    // Decode the menu label back to verify the wide-char round-trip.
    let label_w = &funcs[0].item_name;
    let nul = label_w
        .iter()
        .position(|&u| u == 0)
        .unwrap_or(label_w.len());
    let label = String::from_utf16_lossy(&label_w[..nul]);
    assert_eq!(label, "Insert Hello");
}
