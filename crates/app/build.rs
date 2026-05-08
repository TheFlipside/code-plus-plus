//! Embed `app.manifest` into the Code++ binary on Windows so the app
//! gets Common Controls v6 (rounded buttons, themed EDIT focus
//! accent) and a UTF-8 active codepage. We use the MSVC linker's
//! `/MANIFEST:EMBED` + `/MANIFESTINPUT:` flags rather than a separate
//! `.rc` file or a third-party crate — all the path manipulation
//! happens in build.rs and the manifest stays as a plain XML file
//! checked into the repo.
//!
//! Also embeds `assets/code++.ico` as the application icon. Unlike
//! the manifest, MSVC `link.exe` can't ingest an `.ico` directly —
//! the canonical pipeline is `.ico → .rc → .res → linker`. We
//! generate the trivial one-line `.rc` into `OUT_DIR` rather than
//! checking it in; the only data the resource carries is the
//! `code++.ico` filename and resource id `1`, so the `.rc` itself
//! is pure scaffolding.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Watch the build script itself — without this, Cargo's
    // explicit-rerun-if-changed mode (triggered as soon as any
    // `cargo:rerun-if-changed` is emitted further down) would stop
    // re-running this script on edits, so a tweak to the SDK scan
    // path or the .rc template wouldn't take effect until something
    // else in the watch set changed.
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo"),
    );

    embed_manifest(&manifest_dir);
    embed_app_icon(&manifest_dir);
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
    // as part of the path. A path containing spaces would still
    // round-trip correctly because argv-passing preserves the
    // whole string; only paths containing a literal `"` would
    // genuinely break, and CARGO_MANIFEST_DIR can't contain one.
    let manifest_str = manifest_path.display().to_string();
    println!("cargo:rerun-if-changed={manifest_str}");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{manifest_str}");
}

fn embed_app_icon(manifest_dir: &Path) {
    // crates/app/ → crates/ → repo root → assets/code++.ico
    let ico_path = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crates/app must sit two parents below the repo root")
        .join("assets")
        .join("code++.ico");

    // If the icon hasn't been generated yet, surface a clear
    // diagnostic rather than letting `rc.exe` print a path-not-found
    // error a few lines down. The icon ships in git so this branch
    // is only reachable on a hand-edited workspace where someone
    // deleted the asset.
    if !ico_path.is_file() {
        panic!(
            "missing app icon at {} — regenerate via \
             `python tools/codepp-app-icon/generate.py`",
            ico_path.display()
        );
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR must be set by cargo");
    let rc_path = Path::new(&out_dir).join("app.rc");
    let res_path = Path::new(&out_dir).join("app.res");

    // Resource id `1`: Windows Explorer surfaces the lowest-numbered
    // `ICON` resource as the `.exe`'s file-icon, so picking `1`
    // makes the icon visible both from `LoadIconW(hinstance, MAKEINTRESOURCE(1))`
    // at runtime and from the Explorer file listing without any
    // extra config.
    //
    // The `.rc` parser treats `\` inside double-quoted strings as a
    // C-style escape, so a Windows path like `C:\Users\…\code++.ico`
    // has to be emitted with each `\` doubled. (Forward slashes work
    // too and would side-step the escape, but doubling matches what
    // every other Windows tool produces and reads cleanly in error
    // messages.)
    //
    // `to_str()` rather than `.display()` because `display()` on an
    // extended-length (`\\?\…`) UNC path keeps the prefix literal,
    // which `rc.exe` doesn't understand. Cargo emits
    // `CARGO_MANIFEST_DIR` as valid UTF-8 (Windows paths are
    // natively UTF-16 and Cargo round-trips through UTF-8 cleanly
    // for any path the OS accepts), so the `expect` below is a
    // contract assertion against a Cargo invariant, not a real
    // error path.
    let ico_path_str = ico_path
        .to_str()
        .expect("CARGO_MANIFEST_DIR-derived path must be valid UTF-8");
    let rc_path_string = ico_path_str.replace('\\', "\\\\");
    let rc_content = format!("1 ICON \"{rc_path_string}\"\n");
    std::fs::write(&rc_path, rc_content).expect("write app.rc");

    // `rc.exe` ships with the Windows SDK. The Developer Command
    // Prompt that DEVELOPMENT.md mandates puts it on PATH, but a
    // plain PowerShell with the MSVC env not initialised will not.
    // Try PATH first (the normal case), then fall back to scanning
    // `Windows Kits\10\bin\<latest>\x64\` so a build invoked from
    // any shell that already has cargo on PATH works without
    // additional environment setup.
    let rc_exe = locate_rc_exe();
    let status = Command::new(&rc_exe)
        .args(["/nologo", "/fo"])
        .arg(&res_path)
        .arg(&rc_path)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "failed to launch {} (Windows SDK resource compiler): {e}",
                rc_exe.display()
            )
        });
    if !status.success() {
        panic!(
            "rc.exe exited with {status} compiling {}",
            rc_path.display()
        );
    }

    println!("cargo:rerun-if-changed={}", ico_path.display());
    // Pass the .res file as a linker input. link.exe accepts .res
    // as a positional argument; rustc forwards it via
    // `std::process::Command::arg`, which on Windows handles
    // CRT-style quoting automatically — paths with spaces become
    // `"…"` in the constructed command line, paths without
    // spaces stay bare. Adding our own literal `"` would
    // double-quote and link.exe would see them as part of the
    // filename (LNK1104: cannot open file `"C:\…"`).
    println!("cargo:rustc-link-arg-bins={}", res_path.display());
}

/// Find `rc.exe`. PATH first (Developer Command Prompt has it
/// preconfigured), then a scan of the standard Windows Kits install
/// location (`C:\Program Files (x86)\Windows Kits\10\bin\<sdk-version>\x64\`)
/// so an unconfigured shell still builds.
///
/// Falling back to a path scan keeps the build self-contained — no
/// `cc` build-dep just for one tool lookup, no requirement that
/// every contributor remember to launch the Developer prompt before
/// `cargo build`. The lookup is on the build machine only; never
/// runs at runtime.
fn locate_rc_exe() -> PathBuf {
    // Probe PATH first by constructing a no-arg `where rc.exe`-style
    // check. `Command::new("rc.exe").spawn()` would do the same with
    // an extra process launch on success — checking `which`-style
    // via the program-files scan is comparable cost and more
    // diagnostic-friendly.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    // PATH miss — scan the standard SDK install root for the
    // newest version's `rc.exe` matching the build target's
    // architecture. Directory layout is
    // `<root>\<sdk-version>\<arch>\rc.exe`; `<sdk-version>` follows
    // `10.0.<build>.0`, so a lexicographic max picks the latest.
    //
    // The arch subfolder must match `CARGO_CFG_TARGET_ARCH`: a
    // future `aarch64-pc-windows-msvc` build would need the ARM64
    // `rc.exe`, not the x64 one. (Today the project only targets
    // x86_64-pc-windows-msvc per DEVELOPMENT.md, so the fallback
    // is x64 in practice.)
    let arch_folder = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        // x86_64 → x64; anything else falls through to x64 too as
        // the conservative default (the only Windows targets the
        // SDK ships native rc.exe for are x64/x86/arm64).
        _ => "x64",
    };
    let kits_root = PathBuf::from("C:/Program Files (x86)/Windows Kits/10/bin");
    if let Ok(entries) = std::fs::read_dir(&kits_root) {
        let mut versions: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("10."))
            })
            .collect();
        versions.sort();
        if let Some(latest) = versions.into_iter().next_back() {
            let candidate = latest.join(arch_folder).join("rc.exe");
            // Defence-in-depth: a symlink planted under
            // `Windows Kits\10\bin\` (only writable with admin
            // rights, but a plausible target post-LPE or via a
            // tampered SDK installer) would otherwise satisfy
            // `is_file()` and silently redirect the build. Resolve
            // the candidate and confirm its canonical path stays
            // under `kits_root` before executing it.
            if candidate.is_file() {
                if let Ok(canonical) = candidate.canonicalize() {
                    if let Ok(canonical_root) = kits_root.canonicalize() {
                        if canonical.starts_with(canonical_root) {
                            return canonical;
                        }
                    }
                }
            }
        }
    }

    panic!(
        "could not find rc.exe on PATH or under {}.\n\
         Install the Windows 10 SDK, or run the build from a Developer Command Prompt \
         (DEVELOPMENT.md §2.4) so rc.exe is on PATH.",
        kits_root.display()
    );
}
