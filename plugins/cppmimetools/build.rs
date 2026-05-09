//! Embed a `VS_VERSION_INFO` resource into the plugin DLL on
//! Windows so the Plugin Manager dialog's "Version" column reads
//! the version straight from `Cargo.toml` rather than displaying `—`.
//! No-op on non-Windows targets — Phase 5 brings up the GTK / Cocoa
//! plugin loaders, neither of which uses Win32 PE resources.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    codepp_build_helpers::embed_plugin_version_info("cppmimetools");
}
