//! Embed `app.manifest` into the Code++ binary on Windows so the app
//! gets Common Controls v6 (rounded buttons, themed EDIT focus
//! accent) and a UTF-8 active codepage. We use the MSVC linker's
//! `/MANIFEST:EMBED` + `/MANIFESTINPUT:` flags rather than a separate
//! `.rc` file or a third-party crate — all the path manipulation
//! happens here and the manifest stays as a plain XML file checked
//! into the repo.
//!
//! Also embeds `assets/code++.ico` as the application icon. Unlike
//! the manifest, MSVC `link.exe` can't ingest an `.ico` directly —
//! the canonical pipeline is `.ico → .rc → .res → linker`. The
//! resource-compiler plumbing lives in `codepp-build-helpers` so the
//! in-tree plugins can share it (each plugin emits its own
//! `VS_VERSION_INFO` resource through the same helper).

use std::path::Path;
use std::path::PathBuf;

fn main() {
    // Watch the build script itself — without this, Cargo's
    // explicit-rerun-if-changed mode (triggered as soon as any
    // `cargo:rerun-if-changed` is emitted further down) would stop
    // re-running this script on edits to it.
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo"),
    );

    embed_manifest(&manifest_dir);

    // crates/app/ → crates/ → repo root → assets/code++.ico
    let ico_path = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crates/app must sit two parents below the repo root")
        .join("assets")
        .join("code++.ico");
    codepp_build_helpers::embed_app_icon(&ico_path);
}

fn embed_manifest(manifest_dir: &Path) {
    let manifest_path = manifest_dir.join("app.manifest");

    // Same absolute path used twice — emit it once and reuse so the
    // rerun-if-changed trigger and the linker's input file stay in
    // lockstep if app.manifest ever moves.
    //
    // The path is passed unquoted: rustc forwards each link-arg to
    // link.exe as a single argv element, and link.exe forwards the
    // filename portion to mt.exe verbatim. Wrapping in literal `"`
    // breaks because mt.exe doesn't strip them — it sees the quote
    // as part of the path.
    let manifest_str = manifest_path.display().to_string();
    println!("cargo:rerun-if-changed={manifest_str}");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{manifest_str}");
}
