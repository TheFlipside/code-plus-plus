//! Build-script helpers shared between `crates/app/build.rs` and the
//! in-tree plugin `build.rs` files.
//!
//! The Win32 resource pipeline — author a `.rc`, compile via `rc.exe`
//! to a `.res`, hand the `.res` to the linker — was previously
//! duplicated in `crates/app/build.rs` for the application icon. This
//! crate factors it out so each in-tree plugin can embed a
//! `VS_VERSION_INFO` resource (so the Plugin Manager's "Version"
//! column shows a real number rather than `—`) with a one-line
//! `build.rs`.
//!
//! Every public helper is a no-op on non-Windows targets so a
//! cross-platform `cargo build` (CI runs on Linux + macOS too)
//! keeps working — Phase 5 brings up the GTK / Cocoa backends and
//! their own build-machinery; the version-info resource is a Win32
//! PE concept that doesn't apply elsewhere.
//!
//! Used exclusively from `[build-dependencies]`. Has no runtime
//! footprint.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the resulting `.res` file gets linked. Determines which
/// `cargo:rustc-...-link-arg` directive the helper emits.
#[derive(Clone, Copy)]
pub enum LinkTarget {
    /// Link into binary outputs (`cargo:rustc-link-arg-bins=…`).
    /// What the application's main `.exe` uses.
    Binaries,
    /// Link into cdylib outputs (`cargo:rustc-cdylib-link-arg=…`).
    /// What every Notepad++-compatible plugin uses.
    Cdylib,
}

/// `true` iff the build target is Windows. Helpers short-circuit on
/// non-Windows so the workspace's cross-platform build keeps working.
fn is_windows_target() -> bool {
    std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
}

/// Find `rc.exe`. PATH first (Developer Command Prompt has it
/// preconfigured), then a scan of the standard Windows Kits install
/// location (`C:\Program Files (x86)\Windows Kits\10\bin\<sdk>\<arch>\`)
/// so an unconfigured shell still builds.
///
/// The fallback scan keeps the build self-contained — no `cc`
/// build-dep just for one tool lookup, no requirement that every
/// contributor remember to launch the Developer prompt before
/// `cargo build`. Lookup happens on the build machine only; never
/// runs at runtime.
///
/// Defence-in-depth: the candidate is canonicalised and confirmed to
/// stay under the kits root before being executed, so a symlink
/// planted under `Windows Kits\10\bin\` (only writable with admin
/// rights, but a plausible target post-LPE or via a tampered SDK
/// installer) doesn't redirect the build to an attacker-controlled
/// binary.
///
/// # Panics
///
/// Panics with an actionable message when neither PATH nor the
/// standard Windows Kits install root yields a usable `rc.exe`.
/// Build-time only — the panic surfaces in `cargo build` output
/// and tells the contributor to install the Windows 10 SDK or
/// launch a Developer Command Prompt (DEVELOPMENT.md §2.4).
#[must_use]
pub fn locate_rc_exe() -> PathBuf {
    // PATH branch. Canonicalise the candidate before returning so a
    // later swap (rc.exe replaced between this check and the
    // `Command::new` invocation in `compile_and_link_rc`) is at least
    // narrowed to a single resolved path. PATH itself is implicitly
    // trusted — anyone who can mutate PATH on the build machine
    // already controls the build — but resolving symlinks here keeps
    // the eventual `Command::new` argument deterministic and matches
    // the symmetric defence on the SDK-fallback branch.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return candidate.canonicalize().unwrap_or(candidate);
            }
        }
    }

    // PATH miss — scan the standard SDK install root for the
    // newest version's `rc.exe` matching the build target's
    // architecture. Directory layout is
    // `<root>\<sdk-version>\<arch>\rc.exe`; `<sdk-version>` follows
    // `10.0.<build>.0`, so a lexicographic max picks the latest.
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
            .filter_map(Result::ok)
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

/// Write `rc_text` to `<OUT_DIR>/<stem>.rc`, run `rc.exe` against it
/// to produce `<OUT_DIR>/<stem>.res`, and emit the appropriate
/// `cargo:rustc-...-link-arg=…` directive so the linker pulls the
/// `.res` into the package's output. No-op on non-Windows targets.
///
/// `stem` is purely a filename hint to keep multiple `.rc` files in
/// the same `OUT_DIR` distinct (the app/build.rs uses `app`, plugins
/// use `version-info`).
///
/// # Panics
///
/// Build-time panics when any of the following hold: `OUT_DIR` is
/// unset (Cargo always sets it for build scripts); the `.rc` file
/// can't be written; `rc.exe` can't be launched; or `rc.exe`
/// exits non-zero. Each message names the offending path so the
/// contributor can see what went wrong.
pub fn compile_and_link_rc(rc_text: &str, stem: &str, target: LinkTarget) {
    if !is_windows_target() {
        return;
    }
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR must be set by cargo");
    let rc_path = Path::new(&out_dir).join(format!("{stem}.rc"));
    let res_path = Path::new(&out_dir).join(format!("{stem}.res"));
    std::fs::write(&rc_path, rc_text).unwrap_or_else(|e| {
        panic!("failed to write {}: {e}", rc_path.display());
    });

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
    assert!(
        status.success(),
        "rc.exe exited with {status} compiling {}",
        rc_path.display()
    );

    // Pass the .res file to the linker. Quoting note inherited from
    // the previous app/build.rs: rustc/Command-arg already handles
    // CRT-style quoting on Windows so paths with spaces work, and
    // adding our own literal `"` would break with LNK1104.
    match target {
        LinkTarget::Binaries => {
            println!("cargo:rustc-link-arg-bins={}", res_path.display());
        }
        LinkTarget::Cdylib => {
            println!("cargo:rustc-cdylib-link-arg={}", res_path.display());
        }
    }
}

/// Embed the application icon at resource id 1 (Explorer's file-icon
/// pick is the lowest-numbered ICON resource). `ico_path` must point
/// at a multi-resolution `.ico`; for Code++ that's
/// `assets/code++.ico`, regenerated via
/// `tools/codepp-app-icon/generate.py`.
///
/// No-op on non-Windows targets.
///
/// # Panics
///
/// Build-time panic when `ico_path` doesn't exist on the build
/// machine. The message names the missing path and the generator
/// command so the contributor can self-serve. Also propagates the
/// downstream panics from [`compile_and_link_rc`].
pub fn embed_app_icon(ico_path: &Path) {
    if !is_windows_target() {
        return;
    }
    assert!(
        ico_path.is_file(),
        "missing app icon at {} — regenerate via \
         `python tools/codepp-app-icon/generate.py`",
        ico_path.display()
    );
    println!("cargo:rerun-if-changed={}", ico_path.display());

    // RC parses `\` inside double-quoted strings as a C-style escape,
    // so a Windows path like `C:\Users\…\code++.ico` has to be
    // emitted with each `\` doubled. Use `to_str()` rather than
    // `display()` because `display()` on an extended-length (`\\?\…`)
    // UNC path keeps the prefix literal, which `rc.exe` doesn't
    // understand.
    let ico_str = ico_path
        .to_str()
        .expect("CARGO_MANIFEST_DIR-derived path must be valid UTF-8");
    let escaped = ico_str.replace('\\', "\\\\");
    let rc = format!("1 ICON \"{escaped}\"\n");
    compile_and_link_rc(&rc, "app", LinkTarget::Binaries);
}

/// Embed a `VS_VERSION_INFO` resource into the surrounding crate's
/// cdylib output. Reads `CARGO_PKG_NAME`, `CARGO_PKG_VERSION`,
/// `CARGO_PKG_DESCRIPTION`, `CARGO_PKG_AUTHORS` from the build
/// environment so each in-tree plugin's `build.rs` is a one-liner
/// — no per-plugin `.rc` to maintain, no per-plugin version
/// duplicated outside `Cargo.toml`.
///
/// `dll_stem` is the actual DLL filename without the `.dll`
/// extension (i.e. `[lib].name` from the surrounding `Cargo.toml`).
/// Cargo doesn't expose the cdylib output stem to build scripts
/// (`CARGO_PKG_NAME` is the package name, which often differs from
/// `[lib].name` — Code++'s plugins follow the
/// `codepp-<thing>` / `<thing>` split), so the caller passes it
/// explicitly. It populates the resource's `OriginalFilename`
/// string so Explorer's "Details" tab matches the on-disk file.
///
/// The resulting binary's PE `VS_FIXEDFILEINFO.dwFileVersionMS/LS`
/// fields are populated from the cargo version (parsed into a
/// 4-tuple, missing components default to 0). The Plugin Manager
/// dialog's `read_pe_file_version` then formats them as
/// `MAJOR.MINOR.BUILD.REV` (or the trailing-zero-stripped form) for
/// the "Version" column.
///
/// No-op on non-Windows targets.
///
/// # Panics
///
/// `dll_stem` must be a plain filename component (ASCII alphanumeric,
/// underscore, or hyphen) — anything that could appear in an `[lib]`
/// `name` field. Path separators (`/`, `\`), control characters, or
/// `"` would either escape into a malformed `OriginalFilename` field
/// or, if the contract loosens later, allow injection into the
/// generated `.rc`. Build-time-only contract assertion; rejecting
/// here is preferable to silently producing a broken resource.
pub fn embed_plugin_version_info(dll_stem: &str) {
    assert!(
        !dll_stem.is_empty()
            && dll_stem
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "embed_plugin_version_info: dll_stem must be a plain filename component (ASCII alphanumeric / `_` / `-`); got {dll_stem:?}"
    );

    // Plugin `build.rs` files emit `cargo:rerun-if-changed=build.rs`,
    // which switches Cargo into explicit-rerun mode — the build
    // script no longer re-runs on arbitrary package-file changes.
    // The `CARGO_PKG_*` env vars are derived from `Cargo.toml`, so
    // tell Cargo to re-run the script (and therefore re-emit the
    // embedded `.res`) whenever any of those values change. Without
    // this, bumping the version in `Cargo.toml` would leave the
    // stale version embedded in the DLL until something else
    // triggered the build script. Emitted unconditionally so non-
    // Windows builds bind the same dependency edge — keeps Cargo's
    // dependency graph consistent across platforms.
    println!("cargo:rerun-if-env-changed=CARGO_PKG_NAME");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_DESCRIPTION");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_AUTHORS");

    if !is_windows_target() {
        return;
    }
    let name = std::env::var("CARGO_PKG_NAME").expect("CARGO_PKG_NAME must be set by cargo");
    let version =
        std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION must be set by cargo");
    let description = std::env::var("CARGO_PKG_DESCRIPTION").unwrap_or_default();
    let authors = std::env::var("CARGO_PKG_AUTHORS").unwrap_or_default();

    let (major, minor, build, rev) = parse_version_quad(&version);
    let rc = build_version_info_rc(BuildVersionInfo {
        major,
        minor,
        build,
        rev,
        version_str: &version,
        product_name: &name,
        file_description: &description,
        company_name: &authors,
        original_filename: &format!("{dll_stem}.dll"),
    });
    compile_and_link_rc(&rc, "version-info", LinkTarget::Cdylib);
}

/// Parse a Cargo-style semver string (`"0.1.0"`, `"1.2.3-beta"`,
/// `"2.0"`, …) into a 4-component `(major, minor, build, rev)` tuple.
/// Trailing components default to 0; non-numeric components (a
/// `-beta` suffix on the patch portion) parse the leading digits.
fn parse_version_quad(version: &str) -> (u16, u16, u16, u16) {
    // Strip any pre-release / build-metadata suffix: semver allows
    // `1.2.3-rc1+sha` shapes. `VS_VERSION_INFO` is purely numeric, so
    // we drop everything past the first `-` or `+`.
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core.split('.').map(parse_u16_lenient);
    let a = parts.next().unwrap_or(0);
    let b = parts.next().unwrap_or(0);
    let c = parts.next().unwrap_or(0);
    let d = parts.next().unwrap_or(0);
    (a, b, c, d)
}

/// Parse leading decimal digits from `s` into a `u16`; returns 0
/// when there are no leading digits, and saturates at `u16::MAX` on
/// overflow rather than wrapping. Lenient because build-time
/// metadata strings sometimes get attached to the patch number
/// (`1.2.3post1`) and we'd rather embed `1.2.3.0` than panic, and
/// saturating because a build counter that briefly overshoots 65535
/// should clamp rather than appear as 0.
fn parse_u16_lenient(s: &str) -> u16 {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u32>().map_or(0, |v| {
        // Saturate at u16::MAX. `u16::try_from(...)` after the
        // `.min(...)` would always succeed but reads less
        // directly than the saturating cast — clippy's
        // cast_possible_truncation rule is intentionally
        // allowed here because the `.min` proves the precondition.
        let capped = v.min(u32::from(u16::MAX));
        #[allow(clippy::cast_possible_truncation)]
        let narrowed = capped as u16;
        narrowed
    })
}

#[derive(Clone, Copy)]
struct BuildVersionInfo<'a> {
    major: u16,
    minor: u16,
    build: u16,
    rev: u16,
    version_str: &'a str,
    product_name: &'a str,
    file_description: &'a str,
    company_name: &'a str,
    original_filename: &'a str,
}

/// Build a `VS_VERSION_INFO` `.rc` block from the parsed components.
/// Mirrors the Win32 SDK's documented template — a single
/// `StringFileInfo` block with `040904B0` (US English, Unicode
/// codepage) and the matching `VarFileInfo/Translation` entry. Any
/// modern Windows tool that reads version info finds the strings via
/// this language tag.
fn build_version_info_rc(v: BuildVersionInfo<'_>) -> String {
    // Escape string values so a `"` in a description doesn't break
    // `.rc` parsing and a `\` doesn't trigger C-style escapes. RC
    // syntax accepts `""` as a literal `"` inside quoted strings,
    // and a `\` doesn't need escaping in a string literal (only in
    // path filenames where rc.exe applies C-style escapes).
    let pn = rc_escape(v.product_name);
    let fd = rc_escape(v.file_description);
    let cn = rc_escape(v.company_name);
    let on = rc_escape(v.original_filename);
    let vs = rc_escape(v.version_str);
    let major = v.major;
    let minor = v.minor;
    let build = v.build;
    let rev = v.rev;
    // `FILETYPE 0x2L` = `VFT_DLL`. The version-info reader doesn't
    // actually inspect this field, but authoring it correctly keeps
    // the resource self-consistent for any tool (Explorer's
    // properties dialog, signtool, …) that does. Plugins are the
    // only consumer of this RC template today, hence the literal.
    format!(
        r#"
1 VERSIONINFO
 FILEVERSION    {major},{minor},{build},{rev}
 PRODUCTVERSION {major},{minor},{build},{rev}
 FILEFLAGSMASK  0x3fL
 FILEFLAGS      0x0L
 FILEOS         0x40004L
 FILETYPE       0x2L
 FILESUBTYPE    0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName",      "{cn}"
            VALUE "FileDescription",  "{fd}"
            VALUE "FileVersion",      "{vs}"
            VALUE "InternalName",     "{pn}"
            VALUE "OriginalFilename", "{on}"
            VALUE "ProductName",      "{pn}"
            VALUE "ProductVersion",   "{vs}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    )
}

/// Escape a string for inclusion in an `.rc` `"…"` string literal:
/// `"` is doubled (RC's intra-string escape), `\` is left alone (the
/// surrounding strings are not file-paths so RC's C-style escape
/// handling doesn't apply). Newlines / control bytes are stripped to
/// keep the resource block on one line per VALUE.
fn rc_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            continue;
        }
        if c == '"' {
            out.push_str("\"\"");
        } else {
            out.push(c);
        }
    }
    out
}
