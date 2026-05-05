//! Embed `app.manifest` into the Code++ binary on Windows so the app
//! gets Common Controls v6 (rounded buttons, themed EDIT focus
//! accent) and a UTF-8 active codepage. We use the MSVC linker's
//! `/MANIFEST:EMBED` + `/MANIFESTINPUT:` flags rather than a separate
//! `.rc` file or a third-party crate — all the path manipulation
//! happens in build.rs and the manifest stays as a plain XML file
//! checked into the repo.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let manifest_path = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo"),
    )
    .join("app.manifest");

    // Same absolute path used twice — emit it once and reuse so the
    // rerun-if-changed trigger and the linker's input file stay in
    // lockstep if app.manifest ever moves.
    //
    // The path is passed unquoted: rustc forwards each link-arg to
    // link.exe as a single argv element, and link.exe forwards the
    // filename portion to mt.exe verbatim. Wrapping in literal `"`
    // breaks because mt.exe doesn't strip them — it sees the quote
    // as part of the path. A path containing spaces would still
    // round-trip correctly because argv-passing preserves the
    // whole string; only paths containing a literal `"` would
    // genuinely break, and CARGO_MANIFEST_DIR can't contain one.
    let manifest_str = manifest_path.display().to_string();
    println!("cargo:rerun-if-changed={manifest_str}");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{manifest_str}");
}
