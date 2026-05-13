// Phase 1 build script: compile vendored Scintilla 5.x and Lexilla 5.x as
// static libraries via the `cc` crate. See DESIGN.md §4.1.
//
// Layout:
//   vendor/scintilla/src/*.cxx       — cross-platform editor core (33 files)
//   vendor/scintilla/win32/*.cxx     — Win32 backend (3 files; ScintillaDLL.cxx
//                                      is intentionally excluded — it's the DLL
//                                      entry point, we link statically)
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

    if target_os != "windows" {
        // Phase 5 adds the GTK and Cocoa backends. Until then, non-Windows
        // targets get an empty rlib so the workspace still builds for CI.
        println!(
            "cargo:warning=scintilla-sys: native build is Windows-only in Phase 1; \
             skipping on {target_os}."
        );
        return;
    }

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

    // Cross-platform editor core (alphabetical).
    let core = [
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
    ];
    for f in &core {
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
