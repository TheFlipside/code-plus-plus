// Phase 1 build script: compile vendored Scintilla 5.x and Lexilla 5.x as
// static libraries via the `cc` crate. See DESIGN.md §4.1.
//
// Layout:
//   vendor/scintilla/src/*.cxx       — cross-platform editor core (33 files,
//                                      see `scintilla_core_sources`)
//   vendor/scintilla/win32/*.cxx     — Win32 backend (3 files; ScintillaDLL.cxx
//                                      is intentionally excluded — it's the DLL
//                                      entry point, we link statically)
//   vendor/scintilla/gtk/*.cxx       — GTK 3 backend (3 files + one C
//                                      marshaller). See `build_scintilla_gtk`
//                                      for why GTK 3 rather than GTK 4.
//   vendor/lexilla/src/Lexilla.cxx   — lexer registry entry point
//   vendor/lexilla/lexlib/*.cxx      — lexer base classes (12 files)
//   vendor/lexilla/lexers/Lex*.cxx   — concrete lexers (Phase 4 m1 starter
//                                      set: LexCPP for C/C++, LexRust for
//                                      Rust, LexNull as the no-op fallback.
//                                      Adding more lexers here is a
//                                      one-line append — every Lex*.cxx
//                                      registers itself with Lexilla via
//                                      a global LexerModule constructor,
//                                      so static-linking the file is
//                                      enough to make `CreateLexer("name")`
//                                      find it. The list is kept small to
//                                      bound binary size and Phase 4
//                                      build time; subsequent milestones
//                                      add languages as the demo needs
//                                      them.)
//
// Win32 system libs linked: user32, imm32, ole32, oleaut32, msimg32, gdi32,
// comdlg32, advapi32, comctl32 — required by Scintilla's Win32 backend.

use std::path::{Path, PathBuf};

/// Warning opt-outs applied to every vendored third-party translation
/// unit. See `build_scintilla_gtk` for the per-flag rationale; kept at
/// module scope so the C++ and C builders there cannot drift apart.
const VENDORED_WARNING_OPTOUTS: [&str; 3] = [
    "-Wno-deprecated-declarations",
    "-Wno-cast-function-type",
    "-Wno-unused-parameter",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let scintilla = PathBuf::from("vendor/scintilla");
    let lexilla = PathBuf::from("vendor/lexilla");

    // Sanity check: submodules must be initialised. The error message tells
    // the developer exactly what to run.
    assert!(
        scintilla.join("src/Editor.cxx").exists(),
        "Scintilla submodule missing at {}. Run \
         `git submodule update --init --recursive`.",
        scintilla.display()
    );
    assert!(
        lexilla.join("src/Lexilla.cxx").exists(),
        "Lexilla submodule missing at {}. Run \
         `git submodule update --init --recursive`.",
        lexilla.display()
    );

    println!("cargo:rerun-if-changed=vendor/scintilla/src");
    println!("cargo:rerun-if-changed=vendor/scintilla/win32");
    println!("cargo:rerun-if-changed=vendor/scintilla/include");
    println!("cargo:rerun-if-changed=vendor/lexilla/src");
    println!("cargo:rerun-if-changed=vendor/lexilla/lexlib");
    println!("cargo:rerun-if-changed=vendor/lexilla/lexers");
    println!("cargo:rerun-if-changed=vendor/lexilla/include");
    println!("cargo:rerun-if-changed=cxx/LexillaShim.cxx");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS");

    // Escape hatch for cross-target *type checking*.
    //
    // `cargo check --target x86_64-pc-windows-msvc` from a Linux host
    // is the only way to typecheck `ui_win32`'s ~29k lines without a
    // Windows machine — but it dies here, because build scripts still
    // *run* under `cargo check` and `cc` cannot find `windows.h`. That
    // leaves the largest crate in the workspace verifiable only by
    // pushing to CI, which is how the Phase 5 m1 `pkg_config` breakage
    // reached both non-Linux runners.
    //
    // With this set, the script emits no native library and no link
    // directives. `cargo check` is then fully useful (it never links);
    // `cargo build` will produce an artifact that **cannot link** and
    // must not be shipped, hence the loud warning. Never set this in
    // CI — the runners have real toolchains and must exercise them.
    println!("cargo:rerun-if-env-changed=CODEPP_SKIP_NATIVE_BUILD");
    println!("cargo:rerun-if-env-changed=CI");
    if std::env::var_os("CODEPP_SKIP_NATIVE_BUILD").is_some() {
        // Hard-stop in CI rather than warn. A binary or test target
        // that needs the archive fails at link time with unresolved
        // `scintilla_*` symbols, so the shipped artifact is never at
        // risk — but a library-only job (`cargo check -p codepp-core`,
        // say) links nothing and would exit 0 with the native build
        // silently skipped. That is a false green on the one gate
        // whose whole job is to catch what local builds miss. There is
        // no legitimate reason for a runner to set this: the runners
        // have real toolchains and exist to exercise them.
        assert!(
            std::env::var_os("CI").is_none(),
            "CODEPP_SKIP_NATIVE_BUILD must never be set in CI — it skips the Scintilla and \
             Lexilla native build, which would let a library-only job pass without ever \
             compiling them. Unset it in the workflow or runner environment."
        );
        println!(
            "cargo:warning=CODEPP_SKIP_NATIVE_BUILD is set: skipping the Scintilla/Lexilla \
             native build for {target_os}. Type checking only — anything that actually links \
             (a binary, a test target) will fail with unresolved scintilla_* symbols."
        );
        return;
    }

    match target_os.as_str() {
        "windows" => {
            build_scintilla_win32(&scintilla);
            build_lexilla(&scintilla, &lexilla);

            // Win32 system libraries that Scintilla's Win32 backend depends on.
            // advapi32: registry (Reg*Key APIs in PlatWin.cxx).
            // comctl32: common controls (used by Scintilla's auto-complete and call-tip).
            for lib in &[
                "user32", "imm32", "ole32", "oleaut32", "msimg32", "gdi32", "comdlg32", "advapi32",
                "comctl32",
            ] {
                println!("cargo:rustc-link-lib=dylib={lib}");
            }
        }
        "linux" => {
            println!("cargo:rerun-if-changed=vendor/scintilla/gtk");
            build_scintilla_gtk(&scintilla);
            build_lexilla(&scintilla, &lexilla);
            // GTK's own libraries are emitted as `cargo:rustc-link-lib`
            // lines by `pkg_config::probe_library` inside
            // `build_scintilla_gtk` — no manual list needed here, unlike
            // Win32 where the SDK has no pkg-config equivalent.
        }
        other => {
            // Cocoa lands later in Phase 5. Until then macOS gets an empty
            // rlib so the workspace still builds on the third CI runner.
            println!(
                "cargo:warning=scintilla-sys: no native backend for {other} yet; \
                 skipping the Scintilla build (Cocoa lands in Phase 5)."
            );
        }
    }
}

/// The cross-platform Scintilla editor core, shared verbatim by every
/// backend. Only the platform layer (`win32/`, `gtk/`, later `cocoa/`)
/// differs, so this list is the single source of truth — adding a file
/// here reaches all backends at once.
///
/// Alphabetical, matching the order in `vendor/scintilla/src/`.
fn scintilla_core_sources() -> [&'static str; 33] {
    [
        "AutoComplete",
        "CallTip",
        "CaseConvert",
        "CaseFolder",
        "CellBuffer",
        "ChangeHistory",
        "CharClassify",
        "CharacterCategoryMap",
        "CharacterType",
        "ContractionState",
        "DBCS",
        "Decoration",
        "Document",
        "EditModel",
        "EditView",
        "Editor",
        "Geometry",
        "Indicator",
        "KeyMap",
        "LineMarker",
        "MarginView",
        "PerLine",
        "PositionCache",
        "RESearch",
        "RunStyles",
        "ScintillaBase",
        "Selection",
        "Style",
        "UndoHistory",
        "UniConversion",
        "UniqueString",
        "ViewStyle",
        "XPM",
    ]
}

/// Compile Scintilla's GTK 3 backend.
///
/// **Why GTK 3 and not GTK 4:** Scintilla has no GTK 4 backend. Upstream
/// `vendor/scintilla/doc/ScintillaDoc.html` documents support for "GTK
/// 2.24 and 3.x" only, the highest version guard in `gtk/` is
/// `GTK_CHECK_VERSION(3,22,0)`, and the source uses APIs GTK 4 removed
/// outright (`GdkWindow`, `gtk_widget_get_window`, `gtk_container_add`,
/// `gtk_widget_set_events`, `gdk_window_get_origin`). Targeting GTK 4
/// would mean porting Scintilla's platform layer, which DESIGN.md §1.2
/// rules out as an explicit non-goal. GTK 3.24 is the final, API-frozen
/// GTK 3 series, so this is a stable target rather than a moving one.
fn build_scintilla_gtk(scintilla: &Path) {
    // `probe_library` emits the `cargo:rustc-link-lib` / `-L` lines for
    // GTK and its transitive deps (gdk, glib, gobject, cairo, pango,
    // atk, …) as a side effect, so the Rust link step is handled too.
    let gtk = pkg_config::Config::new()
        .probe("gtk+-3.0")
        .expect("gtk+-3.0 not found. Install libgtk-3-dev (Debian/Ubuntu), gtk3-devel (Fedora), or gtk3 (Arch) — see docs/DEVELOPMENT.md §3.1.");
    // `PlatGTK.cxx` and `ScintillaGTK.cxx` both `#include <gmodule.h>`,
    // and upstream's `gtk/makefile` links `gmodule-2.0` explicitly.
    // Mainstream distros ship that header in the same package as
    // `glib.h`, so omitting this probe happens to work — but relying
    // on that is an undeclared dependency, and the failure mode
    // (missing header on a distro that splits them) is a confusing
    // compile error rather than a clear one.
    let gmodule = pkg_config::Config::new().probe("gmodule-2.0").expect(
        "gmodule-2.0 not found; it ships with glib's dev package — see docs/DEVELOPMENT.md §3.1.",
    );

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .define("GTK", None)
        // GLib raises its macro-deprecation notices through
        // `#pragma GCC warning`, which no `-Wno-*` flag can suppress —
        // this define is the documented opt-out. Needed because
        // ScintillaGTKAccessible.cxx still uses `G_ADD_PRIVATE`'s
        // predecessor; see the flag list below for the wider rationale.
        .define("GLIB_DISABLE_DEPRECATION_WARNINGS", None)
        .include(scintilla.join("include"))
        .include(scintilla.join("src"))
        .include(scintilla.join("gtk"));
    for path in gtk.include_paths.iter().chain(&gmodule.include_paths) {
        build.include(path);
    }
    // Silence warnings originating in vendored third-party C++. These
    // are upstream Scintilla's to fix, not ours, and patching the
    // vendored tree would fork it (DESIGN.md §4.1 pins release tags and
    // expects a clean diff against the upstream tarball). Suppressing
    // them keeps the build output signal-carrying so a warning in code
    // we *do* own is visible. Each flag maps to a real, audited cause:
    //   deprecated-declarations — ATK's pre-`G_ADD_PRIVATE` macros in
    //       ScintillaGTKAccessible.cxx; GLib deprecated but still
    //       supports them.
    //   cast-function-type — the GObject type-system registration
    //       idiom, which casts typed init functions to GInstanceInitFunc
    //       / GInterfaceInitFunc. Load-bearing and correct by GObject's
    //       contract; GCC cannot see that.
    //   unused-parameter — signal handlers matching a GTK callback
    //       signature that ignore some of their arguments.
    for flag in &VENDORED_WARNING_OPTOUTS {
        build.flag_if_supported(flag);
    }

    for f in &scintilla_core_sources() {
        build.file(scintilla.join("src").join(format!("{f}.cxx")));
    }
    // GTK backend. ScintillaGTKAccessible.cxx is required — ScintillaGTK.cxx
    // references `ScintillaGTKAccessible::` symbols directly.
    for f in &["PlatGTK", "ScintillaGTK", "ScintillaGTKAccessible"] {
        build.file(scintilla.join("gtk").join(format!("{f}.cxx")));
    }
    build.compile("scintilla");

    // `scintilla-marshal.c` is plain C (GObject signal marshallers), so
    // it needs its own `cc::Build` — `cpp(true)` applies to the whole
    // builder and compiling C as C++ risks subtle linkage differences.
    let mut marshal = cc::Build::new();
    marshal
        .define("GLIB_DISABLE_DEPRECATION_WARNINGS", None)
        .include(scintilla.join("include"))
        .include(scintilla.join("gtk"));
    for path in gtk.include_paths.iter().chain(&gmodule.include_paths) {
        marshal.include(path);
    }
    // Same vendored-source opt-outs as the C++ builder above — this is
    // glib-genmarshal boilerplate, equally not ours to fix.
    for flag in &VENDORED_WARNING_OPTOUTS {
        marshal.flag_if_supported(flag);
    }
    marshal.file(scintilla.join("gtk").join("scintilla-marshal.c"));
    marshal.compile("scintilla-marshal");
}

fn build_scintilla_win32(scintilla: &Path) {
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .define("STATIC_BUILD", None)
        .define("UNICODE", None)
        .define("_UNICODE", None)
        .define("WIN32_LEAN_AND_MEAN", None)
        .include(scintilla.join("include"))
        .include(scintilla.join("src"))
        .include(scintilla.join("win32"));

    for f in &scintilla_core_sources() {
        build.file(scintilla.join("src").join(format!("{f}.cxx")));
    }

    // Win32 backend. ScintillaDLL.cxx is the DLL entry point and is
    // deliberately omitted (we link statically).
    for f in &["HanjaDic", "PlatWin", "ScintillaWin"] {
        build.file(scintilla.join("win32").join(format!("{f}.cxx")));
    }

    build.compile("scintilla");
}

fn build_lexilla(scintilla: &Path, lexilla: &Path) {
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .define("STATIC_BUILD", None)
        // Lexilla needs Scintilla headers (ILexer.h etc.).
        .include(scintilla.join("include"))
        .include(lexilla.join("include"))
        .include(lexilla.join("lexlib"));

    // Use our slim shim instead of `vendor/lexilla/src/Lexilla.cxx`. The
    // shim implements the same C ABI but only references the lexer
    // modules we statically link below — upstream Lexilla.cxx hardcodes
    // refs to all 125 lexers and would fail to link with `unresolved
    // external symbol lmXxx` for every lexer not in our subset. See
    // `cxx/LexillaShim.cxx` for the maintenance contract.
    build.file("cxx/LexillaShim.cxx");

    // Lexer base classes.
    for f in &[
        "Accessor",
        "CharacterCategory",
        "CharacterSet",
        "DefaultLexer",
        "InList",
        "LexAccessor",
        "LexerBase",
        "LexerModule",
        "LexerSimple",
        "PropSetSimple",
        "StyleContext",
        "WordList",
    ] {
        build.file(lexilla.join("lexlib").join(format!("{f}.cxx")));
    }

    // Concrete lexers — Phase 4 m6 expanded set. Each Lex*.cxx file
    // contains a global `LexerModule` instance whose constructor
    // registers it with Lexilla's catalogue; static-linking the file
    // is therefore sufficient to make `CreateLexer("cpp")` etc.
    // resolve. To add a language, drop its filename in here AND add
    // a row to `crates/core/src/lang.rs::LANG_TABLE`. The two lists
    // must stay in sync — any LangType row whose `lexer` is `Some(_)`
    // refers to a name registered by one of these files.
    //
    // Cross-reference (one Lex*.cxx may register multiple names):
    //   LexBasic    → vb / freebasic / purebasic / blitzbasic / powerbasic
    //   LexHTML     → hypertext / xml / asp / php
    //   LexHex      → hex / srec / tehex
    //   LexProps    → props (also covers ".ini")
    //   LexLisp     → lisp (also used for Scheme via the same lexer)
    //   LexFortran  → fortran (free form) and f77 (fixed form)
    //   LexCPP      → cpp (also covers C, C#, Java, JS, TS, Obj-C, Go, Swift, RC)
    //
    // The list grows when a new LangType is added; performance impact
    // is marginal (each lexer adds ~50–150 KB to the static binary
    // and a single `LexerModule` global construction at startup).
    for f in &[
        "LexAda",
        "LexAsm",
        "LexAsn1",
        "LexAU3",
        "LexAVS",
        "LexBaan",
        "LexBash",
        "LexBasic",
        "LexBatch",
        "LexCOBOL",
        "LexCPP",
        "LexCSS",
        "LexCaml",
        "LexCmake",
        "LexCoffeeScript",
        "LexCrontab",
        "LexCsound",
        "LexD",
        "LexDiff",
        "LexErlang",
        "LexErrorList",
        "LexEScript",
        "LexForth",
        "LexFortran",
        "LexGDScript",
        "LexGui4Cli",
        "LexHTML",
        "LexHaskell",
        "LexHex",
        "LexHollywood",
        "LexInno",
        "LexJSON",
        "LexKix",
        "LexLaTeX",
        "LexLisp",
        "LexLua",
        "LexMMIXAL",
        // Linked but currently unreferenced from `LANG_TABLE` —
        // kept on hand for a future Microsoft Transact-SQL menu
        // entry distinct from the generic SQL one (LexSQL is what
        // the table uses today). Drop this line if the
        // specialisation never lands; the flag-the-deletion comment
        // here makes the intent visible to a cleanup pass.
        "LexMSSQL",
        "LexMake",
        "LexMatlab",
        "LexNim",
        "LexNsis",
        "LexNull",
        "LexOScript",
        "LexPS",
        "LexPascal",
        "LexPerl",
        "LexPowerShell",
        "LexProps",
        "LexPython",
        "LexR",
        "LexRaku",
        "LexRebol",
        "LexRegistry",
        "LexRuby",
        "LexRust",
        "LexSAS",
        "LexSQL",
        "LexSmalltalk",
        "LexSpice",
        "LexTCL",
        "LexTOML",
        "LexTeX",
        "LexTxt2tags",
        "LexVB",
        "LexVHDL",
        "LexVerilog",
        "LexVisualProlog",
        "LexYAML",
    ] {
        build.file(lexilla.join("lexers").join(format!("{f}.cxx")));
    }

    build.compile("lexilla");
}
