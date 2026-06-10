//! Language identification: extension → `LangType`, `LangType` →
//! Lexilla lexer name, plus the data the UI's Language menu reads.
//!
//! `LangType` is a thin newtype over the i32 the Notepad++ plugin ABI
//! uses (`NPPM_GETCURRENTLANGTYPE` and friends). We don't model the
//! enum as a Rust `enum` because plugins are free to set any i32 via
//! `NPPM_SETBUFFERLANGTYPE`, including values from N++ point releases
//! that aren't yet in our compat header — losing those round-trips
//! through a Rust enum would be an ABI break.
//!
//! The set of `pub const`s below covers the entire `LangType_` from
//! `plugins/nppcompat-headers/Notepad_plus_msgs.h`. Numeric values
//! must stay aligned with that header — the static asserts in
//! `tests` catch a drift.
//!
//! # Phase 4 m6 — table-driven design
//!
//! `LANG_TABLE` is the single source of truth for every language
//! Code++ recognises: menu label, plugin-ABI long description,
//! Lexilla lexer name, and the set of file extensions that map onto
//! that language at open time. The accessors (`from_extension`,
//! `lexer_name`, `language_name`, `language_desc`) all walk this
//! table; the UI's Language menu builds itself from the same data.
//! Adding a new language is one row plus a `Lex*.cxx` line in
//! `crates/scintilla-sys/build.rs`.

use std::path::Path;

/// N++-compatible language identifier. Wire-compatible with the i32
/// the plugin ABI passes around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LangType(pub i32);

impl LangType {
    /// The numeric id N++ plugins observe via `NPPM_GETCURRENTLANGTYPE`.
    #[inline]
    #[must_use]
    pub fn as_npp_id(self) -> i32 {
        self.0
    }

    /// Resolve a file extension (without the leading dot, lower-cased
    /// or not — we lower-case internally) to a known `LangType`. Falls
    /// back to [`L_TEXT`] for anything we don't recognise.
    #[must_use]
    pub fn from_extension(ext: &str) -> Self {
        // ASCII-lowercasing avoids allocating for the common case of
        // already-lowercase extensions; collect to String for the rest.
        let lower = if ext.bytes().all(|b| !b.is_ascii_uppercase()) {
            std::borrow::Cow::Borrowed(ext)
        } else {
            std::borrow::Cow::Owned(ext.to_ascii_lowercase())
        };
        for entry in LANG_TABLE {
            for &candidate in entry.extensions {
                if candidate == lower.as_ref() {
                    return entry.lang;
                }
            }
        }
        L_TEXT
    }

    /// Resolve a path to a `LangType` by inspecting its filename then
    /// extension.
    ///
    /// **Filename-pattern matching runs first** — case-insensitive
    /// against every `LangEntry::filenames` entry. Currently this
    /// covers `Makefile` / `GNUmakefile` / `Makefile.in` and the
    /// other Makefile filename variants under `L_MAKEFILE.filenames`;
    /// future commits extend the mechanism to `CMakeLists.txt` /
    /// `Dockerfile` / `Vagrantfile` / dotfiles when those rows are
    /// wired. A `Makefile.in` (extension `.in`, but the basename
    /// matches `Makefile.in` in the filenames list) resolves to
    /// `L_MAKEFILE` even though `.in` is not in
    /// `L_MAKEFILE.extensions` — the filename pattern is more
    /// specific.
    ///
    /// **Extension fallback** runs when no filename pattern matches.
    /// Files with no extension AND no filename match return
    /// [`L_TEXT`].
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            for entry in LANG_TABLE {
                for &candidate in entry.filenames {
                    // `eq_ignore_ascii_case` normalises both sides
                    // for the comparison, so we don't pre-lowercase
                    // `name` the way `from_extension` does for
                    // extensions. Saves an allocation on the hot
                    // path (every file open hits this).
                    if candidate.eq_ignore_ascii_case(name) {
                        return entry.lang;
                    }
                }
            }
        }
        match path.extension().and_then(|s| s.to_str()) {
            Some(ext) => Self::from_extension(ext),
            None => L_TEXT,
        }
    }

    /// The string Lexilla expects in `CreateLexer(name)`. Returns
    /// `None` for [`L_TEXT`] (no lexer attached — Scintilla renders
    /// the buffer in the default style) and for any `LangType` not in
    /// the table (a plugin might set a future N++ enum value via
    /// `NPPM_SETBUFFERLANGTYPE`).
    #[must_use]
    pub fn lexer_name(self) -> Option<&'static str> {
        LANG_TABLE
            .iter()
            .find(|e| e.lang == self)
            .and_then(|e| e.lexer)
    }

    /// Short language name returned by `NPPM_GETLANGUAGENAME`. Notepad++'s
    /// convention is the same string the user sees in the Language menu
    /// ("C", "C++", "Rust", "Normal Text"). Returns `None` for variants
    /// not in the table; the dispatch arm translates that into a
    /// zero-length write so plugins observe "no name available".
    #[must_use]
    pub fn language_name(self) -> Option<&'static str> {
        LANG_TABLE
            .iter()
            .find(|e| e.lang == self)
            .map(|e| e.menu_label)
    }

    /// Long human-readable description returned by `NPPM_GETLANGUAGEDESC`.
    /// Notepad++ uses the longer phrasing here ("C++ source file");
    /// plugins display it in language-pickers and about-dialogs.
    #[must_use]
    pub fn language_desc(self) -> Option<&'static str> {
        LANG_TABLE.iter().find(|e| e.lang == self).map(|e| e.desc)
    }
}

/// One row of [`LANG_TABLE`]. Every language Code++ knows about lives
/// here once; the accessors above and the UI's Language menu both
/// derive their behaviour from this table.
#[derive(Debug, Clone, Copy)]
pub struct LangEntry {
    /// N++-ABI numeric id.
    pub lang: LangType,
    /// Short label shown in the Language menu, also returned by
    /// `NPPM_GETLANGUAGENAME`. N++ convention: title-case English
    /// ("C++", "Rust", "Normal Text").
    pub menu_label: &'static str,
    /// Long description for `NPPM_GETLANGUAGEDESC` ("C++ source file").
    pub desc: &'static str,
    /// String to pass to Lexilla's `CreateLexer(name)`. `None` means
    /// either `L_TEXT` (we want no lexer) or "no Lexilla lexer is the
    /// right match" (the lang is in the menu and round-trips through
    /// the plugin ABI, but the buffer renders without highlighting).
    /// The static-link set in `crates/scintilla-sys/build.rs` must
    /// include the corresponding `Lex*.cxx` for `CreateLexer` to
    /// resolve a `Some(_)` here at runtime.
    pub lexer: Option<&'static str>,
    /// Lower-case file extensions (without the leading dot) that map
    /// to this language. The first match wins; later overlapping
    /// entries are unreachable (they'd need different `LangType`s but
    /// share an extension, which N++ resolves the same way — the
    /// declaration order in this table is the resolution order).
    pub extensions: &'static [&'static str],
    /// Whole-filename patterns (case-insensitive match against the
    /// full file basename) for languages identified by filename
    /// rather than extension. Currently populated for `L_MAKEFILE`
    /// only (`Makefile` / `GNUmakefile` / `Makefile.in` / etc.).
    /// The same mechanism will cover `CMakeLists.txt` /
    /// `Dockerfile` / `Vagrantfile` / dotfiles when those rows are
    /// wired in later Phase 4.5 commits — today's wiring is
    /// Makefile-only. The path-resolution helper
    /// [`LangType::from_path`] checks this list BEFORE falling back
    /// to extension matching, so a file named literally `Makefile`
    /// (no `.ext`) resolves correctly. Empty for languages
    /// identified solely by extension.
    pub filenames: &'static [&'static str],
}

/// Full language table. Sorted alphabetically by `menu_label`
/// (case-insensitive) so the UI's first-letter submenu grouping
/// can iterate in display order. `L_TEXT` sits at index 0 because
/// the menu always shows "Normal Text" at the top, outside the
/// alphabetical block.
///
/// **Lexilla mapping notes:**
/// - `cpp`: covers C, C++, C#, Java, JavaScript, TypeScript,
///   Objective-C, Go, Swift, Resource — N++'s convention is to
///   reuse `LexCPP` for any C-family-ish language with `//` and
///   `/* */` comments and curly-brace blocks.
/// - `hypertext`: HTML/ASP/JSP/PHP — `LexHTML` registers under
///   this name and embeds CSS / JS / PHP / VB-script lexers per
///   tag context.
/// - `hex`: `LexHex` registers all three of `hex` (Intel HEX),
///   `srec` (Motorola S-record), and `tehex` (Tek Extended HEX).
/// - `props`: covers both INI-style `key = value` files and
///   Java-style `.properties`.
pub const LANG_TABLE: &[LangEntry] = &[
    // -- Always-first entry, separated visually in the menu. --
    LangEntry {
        lang: L_TEXT,
        menu_label: "Normal Text",
        desc: "Normal text file",
        lexer: None,
        extensions: &[],
        filenames: &[],
    },
    // -- Alphabetical (case-insensitive). The menu UI groups
    //    same-first-letter blocks of size >= 2 into a submenu titled
    //    by the letter. Single-entry letters stay top-level.       --
    LangEntry {
        lang: L_ADA,
        menu_label: "Ada",
        desc: "Ada source file",
        lexer: Some("ada"),
        extensions: &["ada", "adb", "ads"],
        filenames: &[],
    },
    LangEntry {
        lang: L_ASN1,
        menu_label: "ASN.1",
        desc: "ASN.1 source file",
        lexer: Some("asn1"),
        extensions: &["asn1"],
        filenames: &[],
    },
    LangEntry {
        lang: L_ASP,
        menu_label: "ASP",
        desc: "ASP source file",
        lexer: Some("hypertext"),
        extensions: &["asp"],
        filenames: &[],
    },
    LangEntry {
        lang: L_ASM,
        menu_label: "Assembly",
        desc: "Assembly source file",
        lexer: Some("asm"),
        extensions: &["asm", "s"],
        filenames: &[],
    },
    LangEntry {
        lang: L_AU3,
        menu_label: "AutoIt",
        desc: "AutoIt source file",
        lexer: Some("au3"),
        extensions: &["au3"],
        filenames: &[],
    },
    LangEntry {
        lang: L_AVS,
        menu_label: "AviSynth",
        desc: "AviSynth source file",
        lexer: Some("avs"),
        extensions: &["avs", "avsi"],
        filenames: &[],
    },
    LangEntry {
        lang: L_BAANC,
        menu_label: "BaanC",
        desc: "BaanC source file",
        lexer: Some("baan"),
        extensions: &["baan"],
        filenames: &[],
    },
    LangEntry {
        lang: L_BATCH,
        menu_label: "Batch",
        desc: "Batch file",
        lexer: Some("batch"),
        extensions: &["bat", "cmd"],
        filenames: &[],
    },
    LangEntry {
        lang: L_BLITZBASIC,
        menu_label: "Blitzbasic",
        desc: "Blitzbasic source file",
        lexer: Some("blitzbasic"),
        extensions: &["bb"],
        filenames: &[],
    },
    LangEntry {
        lang: L_C,
        menu_label: "C",
        desc: "C source file",
        lexer: Some("cpp"),
        extensions: &["c", "h"],
        filenames: &[],
    },
    LangEntry {
        lang: L_CS,
        menu_label: "C#",
        desc: "C# source file",
        lexer: Some("cpp"),
        extensions: &["cs"],
        filenames: &[],
    },
    LangEntry {
        lang: L_CPP,
        menu_label: "C++",
        desc: "C++ source file",
        lexer: Some("cpp"),
        extensions: &["cpp", "cxx", "cc", "hpp", "hxx", "hh", "ipp", "tpp", "inl"],
        filenames: &[],
    },
    LangEntry {
        lang: L_CAML,
        menu_label: "Caml",
        desc: "Caml source file",
        lexer: Some("caml"),
        extensions: &["ml", "mli"],
        filenames: &[],
    },
    LangEntry {
        lang: L_CMAKE,
        menu_label: "CMake",
        desc: "CMake source file",
        lexer: Some("cmake"),
        extensions: &["cmake"],
        filenames: &[],
    },
    LangEntry {
        lang: L_COBOL,
        menu_label: "COBOL",
        desc: "COBOL source file",
        // Uppercase intentional: `LexCOBOL.cxx` registers under
        // exactly that string, distinct from every other lexer
        // name in the table (which are lowercase). A
        // well-intentioned "fix" to lowercase here would silently
        // disable highlighting for `.cob`/`.cbl` files.
        lexer: Some("COBOL"),
        extensions: &["cob", "cbl", "cpy"],
        filenames: &[],
    },
    LangEntry {
        lang: L_COFFEESCRIPT,
        menu_label: "CoffeeScript",
        desc: "CoffeeScript source file",
        lexer: Some("coffeescript"),
        extensions: &["coffee", "litcoffee"],
        filenames: &[],
    },
    LangEntry {
        lang: L_CSOUND,
        menu_label: "CSound",
        desc: "CSound source file",
        lexer: Some("csound"),
        extensions: &["orc", "sco", "csd"],
        filenames: &[],
    },
    LangEntry {
        lang: L_CSS,
        menu_label: "CSS",
        desc: "CSS source file",
        lexer: Some("css"),
        extensions: &["css"],
        filenames: &[],
    },
    LangEntry {
        lang: L_D,
        menu_label: "D",
        desc: "D source file",
        lexer: Some("d"),
        extensions: &["d"],
        filenames: &[],
    },
    LangEntry {
        lang: L_DIFF,
        menu_label: "Diff",
        desc: "Diff/patch file",
        lexer: Some("diff"),
        extensions: &["diff", "patch"],
        filenames: &[],
    },
    LangEntry {
        lang: L_ERLANG,
        menu_label: "Erlang",
        desc: "Erlang source file",
        lexer: Some("erlang"),
        extensions: &["erl", "hrl"],
        filenames: &[],
    },
    LangEntry {
        lang: L_ERRORLIST,
        menu_label: "ErrorList",
        desc: "Error-list output file",
        lexer: Some("errorlist"),
        extensions: &[],
        filenames: &[],
    },
    LangEntry {
        lang: L_ESCRIPT,
        menu_label: "ESCRIPT",
        desc: "ESCRIPT source file",
        lexer: Some("escript"),
        extensions: &["em"],
        filenames: &[],
    },
    LangEntry {
        lang: L_FORTH,
        menu_label: "Forth",
        desc: "Forth source file",
        lexer: Some("forth"),
        extensions: &["forth"],
        filenames: &[],
    },
    LangEntry {
        lang: L_FORTRAN_77,
        menu_label: "Fortran (fixed form)",
        desc: "Fortran (fixed form) source file",
        lexer: Some("f77"),
        extensions: &["f", "for", "f77", "ftn"],
        filenames: &[],
    },
    LangEntry {
        lang: L_FORTRAN,
        menu_label: "Fortran (free form)",
        desc: "Fortran (free form) source file",
        lexer: Some("fortran"),
        extensions: &["f90", "f95", "f2k", "f03", "f08", "f15"],
        filenames: &[],
    },
    LangEntry {
        lang: L_FREEBASIC,
        menu_label: "Freebasic",
        desc: "Freebasic source file",
        lexer: Some("freebasic"),
        extensions: &["bas"],
        filenames: &[],
    },
    LangEntry {
        lang: L_GDSCRIPT,
        menu_label: "GDScript",
        desc: "GDScript source file",
        lexer: Some("gdscript"),
        extensions: &["gd"],
        filenames: &[],
    },
    LangEntry {
        lang: L_GOLANG,
        menu_label: "Go",
        desc: "Go source file",
        lexer: Some("cpp"),
        extensions: &["go"],
        filenames: &[],
    },
    LangEntry {
        lang: L_GUI4CLI,
        menu_label: "Gui4Cli",
        desc: "Gui4Cli source file",
        lexer: Some("gui4cli"),
        extensions: &["gc", "gui"],
        filenames: &[],
    },
    LangEntry {
        lang: L_HASKELL,
        menu_label: "Haskell",
        desc: "Haskell source file",
        lexer: Some("haskell"),
        extensions: &["hs"],
        filenames: &[],
    },
    LangEntry {
        lang: L_HOLLYWOOD,
        menu_label: "Hollywood",
        desc: "Hollywood source file",
        lexer: Some("hollywood"),
        extensions: &["hws"],
        filenames: &[],
    },
    LangEntry {
        lang: L_HTML,
        menu_label: "HTML",
        desc: "HTML file",
        lexer: Some("hypertext"),
        extensions: &["html", "htm", "xhtml"],
        filenames: &[],
    },
    LangEntry {
        lang: L_INI,
        menu_label: "INI file",
        desc: "INI file",
        lexer: Some("props"),
        extensions: &["ini"],
        filenames: &[],
    },
    LangEntry {
        lang: L_INNO,
        menu_label: "Inno Setup",
        desc: "Inno Setup script",
        lexer: Some("inno"),
        extensions: &["iss"],
        filenames: &[],
    },
    LangEntry {
        lang: L_IHEX,
        menu_label: "Intel HEX",
        desc: "Intel HEX file",
        // `LexHex.cxx` registers three separate LexerModules
        // (`ihex` / `srec` / `tehex`) — no plain `"hex"` name
        // exists, so the three HEX-format entries each carry their
        // own registered lexer name.
        lexer: Some("ihex"),
        extensions: &["hex", "ihex"],
        filenames: &[],
    },
    LangEntry {
        lang: L_JAVA,
        menu_label: "Java",
        desc: "Java source file",
        lexer: Some("cpp"),
        extensions: &["java"],
        filenames: &[],
    },
    LangEntry {
        lang: L_JAVASCRIPT,
        menu_label: "Javascript",
        desc: "Javascript source file",
        lexer: Some("cpp"),
        extensions: &["js", "mjs", "cjs"],
        filenames: &[],
    },
    LangEntry {
        lang: L_JSON,
        menu_label: "JSON",
        desc: "JSON file",
        lexer: Some("json"),
        extensions: &["json"],
        filenames: &[],
    },
    LangEntry {
        lang: L_JSON5,
        menu_label: "JSON5",
        desc: "JSON5 file",
        // Lexilla's `LexJSON.cxx` registers the `json` lexer that
        // accepts both strict JSON and the JSON5 extensions
        // (single-quoted strings, trailing commas, comments). N++
        // does the same — there's no separate Lexilla
        // registration named `json5`.
        lexer: Some("json"),
        extensions: &["json5"],
        filenames: &[],
    },
    LangEntry {
        lang: L_JSP,
        menu_label: "JSP",
        desc: "JSP source file",
        lexer: Some("hypertext"),
        extensions: &["jsp"],
        filenames: &[],
    },
    LangEntry {
        lang: L_KIX,
        menu_label: "KIXtart",
        desc: "KIXtart source file",
        lexer: Some("kix"),
        extensions: &["kix"],
        filenames: &[],
    },
    LangEntry {
        lang: L_LATEX,
        menu_label: "LaTeX",
        desc: "LaTeX source file",
        lexer: Some("latex"),
        extensions: &["latex"],
        filenames: &[],
    },
    LangEntry {
        lang: L_LISP,
        menu_label: "Lisp",
        desc: "Lisp source file",
        lexer: Some("lisp"),
        extensions: &["lisp", "lsp", "el"],
        filenames: &[],
    },
    LangEntry {
        lang: L_LUA,
        menu_label: "Lua",
        desc: "Lua source file",
        lexer: Some("lua"),
        extensions: &["lua"],
        filenames: &[],
    },
    LangEntry {
        lang: L_MAKEFILE,
        menu_label: "Makefile",
        desc: "Makefile",
        lexer: Some("makefile"),
        // `mak` / `mk` are the conventional Makefile-fragment
        // extensions. `makefile` (the bare word as an "extension")
        // is removed — files literally named `Makefile` have NO
        // extension, so `path.extension()` returns `None` and the
        // entry was unreachable. The whole-filename matching below
        // handles that case correctly.
        extensions: &["mak", "mk"],
        // Whole-filename matching is case-insensitive — `Makefile`,
        // `makefile`, and `MAKEFILE` all hit. Covers every well-
        // known Makefile filename pattern: the bare GNU / BSD forms
        // plus the autotools `.in` / `.am` inputs.
        filenames: &[
            "Makefile",
            "GNUmakefile",
            "BSDmakefile",
            "Makefile.in",
            "Makefile.am",
            "GNUmakefile.in",
        ],
    },
    LangEntry {
        lang: L_MATLAB,
        menu_label: "Matlab",
        desc: "Matlab source file",
        lexer: Some("matlab"),
        extensions: &["matlab"],
        filenames: &[],
    },
    LangEntry {
        lang: L_MMIXAL,
        menu_label: "MMIXAL",
        desc: "MMIXAL source file",
        lexer: Some("mmixal"),
        extensions: &["mms"],
        filenames: &[],
    },
    LangEntry {
        lang: L_NIM,
        menu_label: "Nim",
        desc: "Nim source file",
        lexer: Some("nim"),
        extensions: &["nim"],
        filenames: &[],
    },
    LangEntry {
        lang: L_NNCRONTAB,
        menu_label: "Nncrontab",
        desc: "Nncrontab file",
        lexer: Some("nncrontab"),
        extensions: &["tab"],
        filenames: &[],
    },
    LangEntry {
        lang: L_NSIS,
        menu_label: "NSIS",
        desc: "NSIS script",
        lexer: Some("nsis"),
        extensions: &["nsi", "nsh"],
        filenames: &[],
    },
    LangEntry {
        lang: L_OBJC,
        menu_label: "Objective-C",
        desc: "Objective-C source file",
        lexer: Some("cpp"),
        extensions: &["m", "mm"],
        filenames: &[],
    },
    LangEntry {
        lang: L_OSCRIPT,
        menu_label: "OScript",
        desc: "OScript source file",
        lexer: Some("oscript"),
        extensions: &["osx"],
        filenames: &[],
    },
    LangEntry {
        lang: L_PASCAL,
        menu_label: "Pascal",
        desc: "Pascal source file",
        lexer: Some("pascal"),
        extensions: &["pas", "pp", "p", "dpr"],
        filenames: &[],
    },
    LangEntry {
        lang: L_PERL,
        menu_label: "Perl",
        desc: "Perl source file",
        lexer: Some("perl"),
        extensions: &["pl", "pm", "plx"],
        filenames: &[],
    },
    LangEntry {
        lang: L_PHP,
        menu_label: "PHP",
        desc: "PHP source file",
        lexer: Some("hypertext"),
        extensions: &["php", "phtml"],
        filenames: &[],
    },
    LangEntry {
        lang: L_PS,
        menu_label: "PostScript",
        desc: "PostScript file",
        lexer: Some("ps"),
        extensions: &["ps", "eps"],
        filenames: &[],
    },
    LangEntry {
        lang: L_POWERSHELL,
        menu_label: "PowerShell",
        desc: "PowerShell source file",
        lexer: Some("powershell"),
        extensions: &["ps1", "psm1", "psd1"],
        filenames: &[],
    },
    LangEntry {
        lang: L_PROPS,
        menu_label: "Properties",
        desc: "Properties file",
        lexer: Some("props"),
        extensions: &["properties"],
        filenames: &[],
    },
    LangEntry {
        lang: L_PUREBASIC,
        menu_label: "Purebasic",
        desc: "Purebasic source file",
        lexer: Some("purebasic"),
        extensions: &["pb"],
        filenames: &[],
    },
    LangEntry {
        lang: L_PYTHON,
        menu_label: "Python",
        desc: "Python source file",
        lexer: Some("python"),
        extensions: &["py", "pyw"],
        filenames: &[],
    },
    LangEntry {
        lang: L_R,
        menu_label: "R",
        desc: "R source file",
        lexer: Some("r"),
        extensions: &["r"],
        filenames: &[],
    },
    LangEntry {
        lang: L_RAKU,
        menu_label: "Raku",
        desc: "Raku source file",
        lexer: Some("raku"),
        extensions: &["raku", "rakumod"],
        filenames: &[],
    },
    LangEntry {
        lang: L_REBOL,
        menu_label: "REBOL",
        desc: "REBOL source file",
        lexer: Some("rebol"),
        extensions: &["reb", "rebol"],
        filenames: &[],
    },
    LangEntry {
        lang: L_REGISTRY,
        menu_label: "Registry",
        desc: "Windows Registry file",
        lexer: Some("registry"),
        extensions: &["reg"],
        filenames: &[],
    },
    LangEntry {
        lang: L_RC,
        menu_label: "Resource file",
        desc: "Resource source file",
        lexer: Some("cpp"),
        extensions: &["rc"],
        filenames: &[],
    },
    LangEntry {
        lang: L_RUBY,
        menu_label: "Ruby",
        desc: "Ruby source file",
        lexer: Some("ruby"),
        extensions: &["rb", "rbw"],
        filenames: &[],
    },
    LangEntry {
        lang: L_RUST,
        menu_label: "Rust",
        desc: "Rust source file",
        lexer: Some("rust"),
        extensions: &["rs"],
        filenames: &[],
    },
    LangEntry {
        lang: L_SREC,
        menu_label: "S-Record",
        desc: "Motorola S-Record file",
        lexer: Some("srec"),
        extensions: &["srec", "s19", "s28", "s37"],
        filenames: &[],
    },
    LangEntry {
        lang: L_SAS,
        menu_label: "SAS",
        desc: "SAS source file",
        lexer: Some("sas"),
        extensions: &["sas"],
        filenames: &[],
    },
    LangEntry {
        lang: L_SCHEME,
        menu_label: "Scheme",
        desc: "Scheme source file",
        lexer: Some("lisp"),
        extensions: &["scm", "ss"],
        filenames: &[],
    },
    LangEntry {
        lang: L_BASH,
        menu_label: "Shell",
        desc: "Shell script",
        lexer: Some("bash"),
        extensions: &["sh", "bash"],
        filenames: &[],
    },
    LangEntry {
        lang: L_SMALLTALK,
        menu_label: "Smalltalk",
        desc: "Smalltalk source file",
        lexer: Some("smalltalk"),
        extensions: &["st"],
        filenames: &[],
    },
    LangEntry {
        lang: L_SPICE,
        menu_label: "Spice",
        desc: "Spice circuit file",
        lexer: Some("spice"),
        extensions: &["sp", "spice"],
        filenames: &[],
    },
    LangEntry {
        lang: L_SQL,
        menu_label: "SQL",
        desc: "SQL source file",
        // Generic SQL via `LexSQL.cxx`. The `mssql` lexer (from
        // `LexMSSQL.cxx`) is also linked into the binary for any
        // future Microsoft Transact-SQL specialisation but is not
        // referenced from this table — N++'s public LangType_
        // enum doesn't carry a separate id for T-SQL.
        lexer: Some("sql"),
        extensions: &["sql"],
        filenames: &[],
    },
    LangEntry {
        lang: L_SWIFT,
        menu_label: "Swift",
        desc: "Swift source file",
        lexer: Some("cpp"),
        extensions: &["swift"],
        filenames: &[],
    },
    LangEntry {
        lang: L_TCL,
        menu_label: "TCL",
        desc: "TCL source file",
        lexer: Some("tcl"),
        extensions: &["tcl"],
        filenames: &[],
    },
    LangEntry {
        lang: L_TEHEX,
        menu_label: "Tektronix extended HEX",
        desc: "Tektronix Extended HEX file",
        lexer: Some("tehex"),
        extensions: &["tek"],
        filenames: &[],
    },
    LangEntry {
        lang: L_TEX,
        menu_label: "TeX",
        desc: "TeX source file",
        lexer: Some("tex"),
        extensions: &["tex"],
        filenames: &[],
    },
    LangEntry {
        lang: L_TOML,
        menu_label: "TOML",
        desc: "TOML file",
        lexer: Some("toml"),
        extensions: &["toml"],
        filenames: &[],
    },
    LangEntry {
        lang: L_TXT2TAGS,
        menu_label: "txt2tags",
        desc: "txt2tags source file",
        lexer: Some("txt2tags"),
        extensions: &["t2t"],
        filenames: &[],
    },
    LangEntry {
        lang: L_TYPESCRIPT,
        menu_label: "TypeScript",
        desc: "TypeScript source file",
        lexer: Some("cpp"),
        extensions: &["ts", "tsx"],
        filenames: &[],
    },
    LangEntry {
        lang: L_VERILOG,
        menu_label: "Verilog",
        desc: "Verilog source file",
        lexer: Some("verilog"),
        extensions: &["v", "vh", "sv", "svh"],
        filenames: &[],
    },
    LangEntry {
        lang: L_VHDL,
        menu_label: "VHDL",
        desc: "VHDL source file",
        lexer: Some("vhdl"),
        extensions: &["vhd", "vhdl"],
        filenames: &[],
    },
    LangEntry {
        lang: L_VB,
        menu_label: "Visual Basic",
        desc: "Visual Basic source file",
        lexer: Some("vb"),
        extensions: &["vb", "vbs"],
        filenames: &[],
    },
    LangEntry {
        lang: L_VISUALPROLOG,
        menu_label: "Visual Prolog",
        desc: "Visual Prolog source file",
        lexer: Some("visualprolog"),
        extensions: &["vip"],
        filenames: &[],
    },
    LangEntry {
        lang: L_XML,
        menu_label: "XML",
        desc: "XML file",
        lexer: Some("xml"),
        extensions: &["xml", "xsd", "xsl", "xslt", "svg"],
        filenames: &[],
    },
    LangEntry {
        lang: L_YAML,
        menu_label: "YAML",
        desc: "YAML file",
        lexer: Some("yaml"),
        extensions: &["yaml", "yml"],
        filenames: &[],
    },
];

// --- Numeric ids must match `LangType_` in
// plugins/nppcompat-headers/Notepad_plus_msgs.h verbatim. The header
// is the public ABI; this module mirrors it.
//
// **Variants intentionally absent from `LANG_TABLE`** (so
// `language_name`/`lexer_name`/`language_desc` return `None`):
// - `L_ASCII` (id 14), `L_USER` (id 15) — N++'s "user-defined
//   language" slots; covered by the greyed UDL submenu in the UI
//   pending Phase 5 user-defined-language support.
// - `L_FLASH` (id 27) — ActionScript / Flash; no Lexilla lexer
//   that maps to N++'s `LexFlash` registration. Out of menu scope.
// - `L_JS` (id 19) — N++ keeps two JavaScript ids: this older
//   `L_JS` and the newer `L_JAVASCRIPT` (id 58). Code++ resolves
//   `.js` extensions to `L_JAVASCRIPT` (the canonical entry); a
//   plugin can still set `L_JS` via `NPPM_SETBUFFERLANGTYPE` and
//   it round-trips through the i32 boundary.
// - `L_SEARCHRESULT` (id 47) — N++ uses this for its own search-
//   results panel; Code++'s FIF dock has its own model. Not a
//   user-pickable language.
// - `L_EXTERNAL` (id 93) — placeholder for plugin-defined external
//   lexers; no built-in mapping.
//
// A future contributor adding a row for one of these should
// double-check the menu / detector implications before bringing
// it back into scope.

pub const L_TEXT: LangType = LangType(0);
pub const L_PHP: LangType = LangType(1);
pub const L_C: LangType = LangType(2);
pub const L_CPP: LangType = LangType(3);
pub const L_CS: LangType = LangType(4);
pub const L_OBJC: LangType = LangType(5);
pub const L_JAVA: LangType = LangType(6);
pub const L_RC: LangType = LangType(7);
pub const L_HTML: LangType = LangType(8);
pub const L_XML: LangType = LangType(9);
pub const L_MAKEFILE: LangType = LangType(10);
pub const L_PASCAL: LangType = LangType(11);
pub const L_BATCH: LangType = LangType(12);
pub const L_INI: LangType = LangType(13);
pub const L_ASCII: LangType = LangType(14);
pub const L_USER: LangType = LangType(15);
pub const L_ASP: LangType = LangType(16);
pub const L_SQL: LangType = LangType(17);
pub const L_VB: LangType = LangType(18);
pub const L_JS: LangType = LangType(19);
pub const L_CSS: LangType = LangType(20);
pub const L_PERL: LangType = LangType(21);
pub const L_PYTHON: LangType = LangType(22);
pub const L_LUA: LangType = LangType(23);
pub const L_TEX: LangType = LangType(24);
pub const L_FORTRAN: LangType = LangType(25);
pub const L_BASH: LangType = LangType(26);
pub const L_FLASH: LangType = LangType(27);
pub const L_NSIS: LangType = LangType(28);
pub const L_TCL: LangType = LangType(29);
pub const L_LISP: LangType = LangType(30);
pub const L_SCHEME: LangType = LangType(31);
pub const L_ASM: LangType = LangType(32);
pub const L_DIFF: LangType = LangType(33);
pub const L_PROPS: LangType = LangType(34);
pub const L_PS: LangType = LangType(35);
pub const L_RUBY: LangType = LangType(36);
pub const L_SMALLTALK: LangType = LangType(37);
pub const L_VHDL: LangType = LangType(38);
pub const L_KIX: LangType = LangType(39);
pub const L_AU3: LangType = LangType(40);
pub const L_CAML: LangType = LangType(41);
pub const L_ADA: LangType = LangType(42);
pub const L_VERILOG: LangType = LangType(43);
pub const L_MATLAB: LangType = LangType(44);
pub const L_HASKELL: LangType = LangType(45);
pub const L_INNO: LangType = LangType(46);
pub const L_SEARCHRESULT: LangType = LangType(47);
pub const L_CMAKE: LangType = LangType(48);
pub const L_YAML: LangType = LangType(49);
pub const L_COBOL: LangType = LangType(50);
pub const L_GUI4CLI: LangType = LangType(51);
pub const L_D: LangType = LangType(52);
pub const L_POWERSHELL: LangType = LangType(53);
pub const L_R: LangType = LangType(54);
pub const L_JSP: LangType = LangType(55);
pub const L_COFFEESCRIPT: LangType = LangType(56);
pub const L_JSON: LangType = LangType(57);
pub const L_JAVASCRIPT: LangType = LangType(58);
pub const L_FORTRAN_77: LangType = LangType(59);
pub const L_BAANC: LangType = LangType(60);
pub const L_SREC: LangType = LangType(61);
pub const L_IHEX: LangType = LangType(62);
pub const L_TEHEX: LangType = LangType(63);
pub const L_SWIFT: LangType = LangType(64);
pub const L_ASN1: LangType = LangType(65);
pub const L_AVS: LangType = LangType(66);
pub const L_BLITZBASIC: LangType = LangType(67);
pub const L_PUREBASIC: LangType = LangType(68);
pub const L_FREEBASIC: LangType = LangType(69);
pub const L_CSOUND: LangType = LangType(70);
pub const L_ERLANG: LangType = LangType(71);
pub const L_ESCRIPT: LangType = LangType(72);
pub const L_FORTH: LangType = LangType(73);
pub const L_LATEX: LangType = LangType(74);
pub const L_MMIXAL: LangType = LangType(75);
pub const L_NIM: LangType = LangType(76);
pub const L_NNCRONTAB: LangType = LangType(77);
pub const L_OSCRIPT: LangType = LangType(78);
pub const L_REBOL: LangType = LangType(79);
pub const L_REGISTRY: LangType = LangType(80);
pub const L_RUST: LangType = LangType(81);
pub const L_SPICE: LangType = LangType(82);
pub const L_TXT2TAGS: LangType = LangType(83);
pub const L_VISUALPROLOG: LangType = LangType(84);
pub const L_TYPESCRIPT: LangType = LangType(85);
pub const L_GDSCRIPT: LangType = LangType(86);
pub const L_HOLLYWOOD: LangType = LangType(87);
pub const L_GOLANG: LangType = LangType(88);
pub const L_RAKU: LangType = LangType(89);
pub const L_TOML: LangType = LangType(90);
pub const L_SAS: LangType = LangType(91);
pub const L_ERRORLIST: LangType = LangType(92);
pub const L_EXTERNAL: LangType = LangType(93);
/// Notepad++ added JSON5 as a distinct language id in a recent
/// release; numeric value `94` matches the upstream public ABI as
/// of the Lexilla 5.x line that ships with the latest N++ stable.
/// Kept distinct from [`L_JSON`] so the menu can show two
/// alphabetically-adjacent entries (`JSON` / `JSON5`) and a
/// plugin can address either independently via
/// `NPPM_SETBUFFERLANGTYPE`.
pub const L_JSON5: LangType = LangType(94);

/// Space-separated primary keyword list for C, installed via
/// `SCI_SETKEYWORDS(0, ...)` (the `LexCPP` lexer's `SCE_C_WORD` class).
/// Control-flow keywords, storage-class specifiers, and other
/// non-type reserved words. Primitive type names (`int`, `char`,
/// `float`, etc.) live in [`C_KEYWORDS_2`] so they pick up the
/// distinct steel-blue `SCE_C_WORD2` colour matching Notepad++'s
/// default rendering.
///
/// Covers C89 through C23 reserved words: the original C89/C99/C11
/// set, the `_`-prefixed C99/C11 forms (`_Alignas`, `_Atomic`, ...),
/// and the C23 lowercase aliases (`alignas`, `static_assert`,
/// `thread_local`) plus the C23 additions `constexpr`, `nullptr`,
/// `true`, `false`, `typeof`, `typeof_unqual`.
///
/// **A word in both class 0 and class 1 takes class 0's colour** —
/// `LexCPP`'s classifier checks class 0 first. So primitives must be
/// moved here only if they should NOT pick up the secondary colour;
/// any word in both lists is wasted bytes.
pub const C_KEYWORDS: &str = concat!(
    "auto break case const continue default do else enum extern for goto if ",
    "inline register restrict return sizeof static struct switch typedef ",
    "union volatile while ",
    // C99/C11 underscore-prefixed forms.
    "_Alignas _Alignof _Atomic _Generic _Noreturn _Static_assert _Thread_local ",
    // C23 additions: language constants, type-introspection, and the
    // lowercase aliases for the older `_`-prefixed keywords (the
    // underscored forms above remain valid; both render the same
    // because the lexer matches whole tokens).
    "alignas alignof constexpr false nullptr static_assert thread_local true ",
    "typeof typeof_unqual"
);

/// Space-separated secondary (type) keyword list for C, installed via
/// `SCI_SETKEYWORDS(1, ...)` (`LexCPP`'s `SCE_C_WORD2` class). Primitive
/// type names and type modifiers. Mapped to `StyleSlot::Keyword2`
/// (steel blue) in the host theme so types render distinctly from
/// control-flow keywords — same as Notepad++'s C / C++ default.
pub const C_KEYWORDS_2: &str = concat!(
    "char double float int long short signed unsigned void ",
    // C99 underscore-prefixed primitive types.
    "_Bool _Complex _Imaginary ",
    // C23 additions: `bool` (the lowercase alias for `_Bool`, now a
    // proper keyword), and `_BitInt` (bit-precise integer type, e.g.
    // `_BitInt(7)`).
    "bool _BitInt"
);

/// Space-separated primary keyword list for C++. Reserved words
/// through C++23 minus the primitive type aliases. The same class-0
/// vs class-1 split as [`C_KEYWORDS`] / [`C_KEYWORDS_2`]; primitive
/// types live in [`CPP_KEYWORDS_2`] so they pick up `SCE_C_WORD2`'s
/// distinct colour.
///
/// Includes the C++20 module declarators `import` / `module`, the
/// C++20 coroutine keywords (`co_await` / `co_return` / `co_yield`),
/// the C++20 concepts vocabulary (`concept` / `requires`), and the
/// C++20 immediate / persistent function specifiers (`consteval` /
/// `constinit`).
pub const CPP_KEYWORDS: &str = concat!(
    "alignas alignof and and_eq asm auto bitand bitor break case catch class ",
    "compl concept const consteval constexpr constinit const_cast continue ",
    "co_await co_return co_yield decltype default delete do dynamic_cast else ",
    "enum explicit export extern false for friend goto if import inline module ",
    "mutable namespace new noexcept not not_eq nullptr operator or or_eq ",
    "private protected public register reinterpret_cast requires return sizeof ",
    "static static_assert static_cast struct switch template this thread_local ",
    "throw true try typedef typeid typename union using virtual volatile while ",
    "xor xor_eq"
);

/// Space-separated secondary (type) keyword list for C++. Primitive
/// type names — superset of [`C_KEYWORDS_2`] adding `bool` (proper
/// C++ keyword, unlike C's `_Bool`-with-`<stdbool.h>` story),
/// `wchar_t`, and the C++20 UTF character types
/// (`char8_t` / `char16_t` / `char32_t`). Installed via
/// `SCI_SETKEYWORDS(1, ...)` for `SCE_C_WORD2` colouring.
pub const CPP_KEYWORDS_2: &str = concat!(
    "bool char char8_t char16_t char32_t double float int long short signed ",
    "unsigned void wchar_t"
);

/// Space-separated primary keyword list for Objective-C. Installed
/// via the `LexCPP` lexer's `SCI_SETKEYWORDS(0, ...)` for `SCE_C_WORD`
/// (the blue "Keyword" slot). Pair with [`OBJC_KEYWORDS_2`] in class 1
/// for the type vocabulary.
///
/// Objective-C is a strict superset of C, so class 0 includes the
/// full C control-flow / storage-class / qualifier vocabulary plus
/// C11 underscore-prefixed keywords (`_Alignas` / `_Atomic` / etc.).
/// The Objective-C-specific additions split into seven categories:
///
///   1. **Directive identifiers** — `interface` / `implementation` /
///      `end` / `class` / `protocol` / `property` / `synthesize` /
///      `dynamic` / `selector` / `encode` / `defs` /
///      `compatibility_alias` / `try` / `catch` / `throw` /
///      `finally` / `synchronized` / `autoreleasepool` / `public` /
///      `protected` / `private` / `package` / `optional` / `required`
///      / `import` / `available`. Listed **without** the leading `@`
///      because `LexCPP` doesn't treat `@` as an identifier char —
///      `@interface` tokenises as two tokens (the `@` styled as
///      `SCE_C_OPERATOR`, the identifier `interface` looked up against
///      the wordlist). Same approach Notepad++'s `objc` row uses.
///   2. **Method parameter qualifiers** (Distributed Objects
///      vocabulary) — `in` / `out` / `inout` / `oneway` / `bycopy` /
///      `byref`. Niche in modern code but every Objective-C-aware
///      editor still colours them.
///   3. **ARC ownership qualifiers** — `__strong` / `__weak` /
///      `__unsafe_unretained` / `__autoreleasing` and the bridge-cast
///      family `__bridge` / `__bridge_transfer` / `__bridge_retained`.
///      The leading underscores are identifier characters in `LexCPP`
///      so each tokenises as a single identifier.
///   4. **Block specifier** — `__block` (captured-variable annotation
///      in block expressions).
///   5. **Constants** — `YES` / `NO` / `nil` / `Nil` / `NULL` /
///      `true` / `false`. Casing matters (`LexCPP` is case-sensitive;
///      `yes` would not match `YES`).
///   6. **Contextual identifiers coloured keyword-blue by every
///      editor** — `self` / `super`. Technically `self` is an
///      implicit method parameter and `super` is a contextual message
///      receiver, but Xcode / Notepad++ / VS Code all paint them as
///      keywords.
///   7. **Type-introspection operator** — `__typeof` / `__typeof__`
///      (GCC/Clang extensions widely used in `weakify`/`strongify`
///      macros). Sit alongside `sizeof` / `_Alignof` as type-query
///      operators.
///
/// Library identifiers (`NSObject` / `NSString` / `UIView` /
/// `NSInteger` / `CGFloat` / ...) are deliberately omitted — they are
/// framework vocabulary, not language vocabulary.
///
/// Accepted false-positive risk: `in` / `out` / `available` /
/// `property` are valid bare variable names in real Objective-C code
/// (e.g. `NSError **out = nil;`, `BOOL available = ...`). Notepad++
/// and Xcode accept the trade-off — they colour the directive form
/// at the cost of mis-colouring rare same-named variables. Code++
/// follows that established baseline rather than under-colouring the
/// directives which are far more common.
///
/// Sourced and adversarially verified across three lenses (Apple
/// spec / production iOS+macOS code / editor baselines).
pub const OBJC_KEYWORDS: &str = concat!(
    "__autoreleasing __block __bridge __bridge_retained __bridge_transfer ",
    "__strong __typeof __typeof__ __unsafe_unretained __weak NO NULL Nil YES ",
    "_Alignas _Alignof _Atomic _Generic _Noreturn _Static_assert _Thread_local ",
    "auto autoreleasepool available break bycopy byref case catch class ",
    "compatibility_alias const continue default defs do dynamic else encode ",
    "end enum extern false finally for goto if implementation import in inline ",
    "inout interface nil oneway optional out package private property protected ",
    "protocol public register required restrict return selector self sizeof ",
    "static struct super switch synchronized synthesize throw true try typedef ",
    "union volatile while"
);

/// Space-separated secondary (type) keyword list for Objective-C.
/// Installed via `SCI_SETKEYWORDS(1, ...)` for `SCE_C_WORD2` colouring.
/// Four categories:
///
///   1. **Objective-C type vocabulary** — `id` (any object), `Class`
///      (class object), `SEL` (selector), `IMP` (method
///      implementation function pointer), `BOOL` (boolean),
///      `instancetype` (return-self type, Modern Objective-C),
///      `Method` (`<objc/runtime.h>` opaque), `Ivar`
///      (`<objc/runtime.h>` opaque), `Protocol` (runtime protocol
///      class).
///   2. **Nullability qualifiers** (clang 3.7+) — `_Nullable` /
///      `_Nonnull` / `_Null_unspecified`. Underscore-prefix forms;
///      the macro spellings (`nullable` / `nonnull` /
///      `null_unspecified`) are intentionally NOT listed here
///      because they would mis-colour user-named identifiers.
///   3. **Lightweight-generics variance qualifiers** (Modern
///      Objective-C, iOS 9+) — `__kindof` (covariant-allowing-
///      subclass), `__covariant`, `__contravariant`.
///   4. **C primitive types** — `char` / `short` / `int` / `long` /
///      `float` / `double` / `signed` / `unsigned` / `void` / `bool`
///      / `_Bool` / `_Complex` / `_Imaginary` / `_BitInt`.
///      Objective-C is a strict C superset, so the full C primitive
///      vocabulary applies. Mirrors the [`C_KEYWORDS_2`] / [`CPP_KEYWORDS_2`]
///      class-1 contents — same blue-vs-steel-blue rendering as the
///      rest of the `LexCPP` family.
pub const OBJC_KEYWORDS_2: &str = concat!(
    "BOOL Class IMP Ivar Method Protocol SEL _BitInt _Bool _Complex _Imaginary ",
    "_Nonnull _Null_unspecified _Nullable __contravariant __covariant __kindof ",
    "bool char double float id instancetype int long short signed unsigned void"
);

/// Space-separated primary keyword list for Java. Installed via the
/// `LexCPP` lexer's `SCI_SETKEYWORDS(0, ...)` for `SCE_C_WORD` (the
/// blue "Keyword" slot). Pair with [`JAVA_KEYWORDS_2`] in class 1
/// for the primitive types + `var`.
///
/// Four categories:
///
///   1. **JLS §3.9 reserved words** (41) — control-flow (`if`/`else`/
///      `switch`/`case`/`break`/`continue`/`return`/`for`/`while`/`do`/
///      `try`/`catch`/`finally`/`throw`/`throws`), declarations
///      (`class`/`interface`/`enum`/`package`/`import`/`extends`/
///      `implements`/`this`/`super`), modifiers (`abstract`/`final`/
///      `native`/`private`/`protected`/`public`/`static`/`strictfp`/
///      `synchronized`/`transient`/`volatile`), operators (`new`/
///      `instanceof`/`assert`/`default`), and the never-implemented-
///      but-still-reserved `const` / `goto` (JLS §3.9 reserves them
///      so they can't be used as identifiers).
///   2. **Modern contextual keywords** (5) — `yield` (Java 14 switch
///      expressions), `record` (Java 14), `sealed` / `permits`
///      (Java 17), `when` (Java 21 pattern guards). Contextual per
///      the JLS but coloured globally by every editor.
///   3. **Java 9+ module-system restricted identifiers** (9) —
///      `module` / `exports` / `requires` / `opens` / `uses` /
///      `provides` / `to` / `with` / `transitive`. Reserved only
///      inside `module-info.java` but coloured globally by Notepad++
///      / `IntelliJ` / Eclipse / VS Code.
///   4. **Literal constants** (3) — `true` / `false` / `null`.
///      JLS classifies these as `BooleanLiteral` / `NullLiteral`
///      rather than keywords, but every editor renders them
///      keyword-blue.
///
/// **Deliberately excluded:**
///   - **`non-sealed`** (Java 17 hyphenated keyword): the hyphen
///     breaks identifier-shape tokenisation — Lexilla wordlists
///     match identifier tokens only, so the lexer would never match
///     it. Real Java code with `non-sealed` will see `non` lexed as
///     an identifier and `sealed` as a keyword; same trade-off
///     Notepad++ accepts.
///   - **Library identifiers** (`String`, `Object`, `System`, `List`,
///     `ArrayList`, `Math`, `Integer`, ...): standard-library
///     vocabulary, not language vocabulary.
///
/// **Accepted false-positive risk:** all nine module-system
/// identifiers (`module` / `exports` / `requires` / `opens` /
/// `uses` / `provides` / `to` / `with` / `transitive`) and the
/// contextual keywords (`yield` / `when`) are legal identifiers
/// outside their reserved context. Colouring them globally would
/// mis-render a `String with = ...;` variable declaration as a
/// partly-keyword line. Notepad++ / `IntelliJ` / Eclipse all accept
/// this trade-off — the directive form is far more common than the
/// identifier form. Code++ follows that baseline.
///
/// Sourced and adversarially verified across three lenses
/// (JLS spec / production code / editor baselines).
pub const JAVA_KEYWORDS: &str = concat!(
    "abstract assert break case catch class const continue default do else ",
    "enum exports extends false final finally for goto if implements import ",
    "instanceof interface module native new null opens package permits private ",
    "protected provides public record requires return sealed static strictfp ",
    "super switch synchronized this throw throws to transient transitive true ",
    "try uses volatile when while with yield"
);

/// Space-separated secondary (type) keyword list for Java. Installed
/// via `SCI_SETKEYWORDS(1, ...)` for `SCE_C_WORD2` (the steel-blue
/// "Keyword2" slot). Two categories:
///
///   1. **Primitive types + `void`** (9) — `boolean` / `byte` /
///      `short` / `char` / `int` / `long` / `float` / `double` /
///      `void` (JLS §4.2 primitives + the §8.4.5 void return type).
///      Steel-blue rendering matches Notepad++'s `type1` row for
///      Java.
///   2. **Type-inference contextual keyword** — `var` (Java 10).
///      Classed with types because it visually represents a type at
///      the inference site, mirroring the C# precedent for `var` /
///      `dynamic` in [`CS_KEYWORDS_2`].
pub const JAVA_KEYWORDS_2: &str = "boolean byte char double float int long short var void";

/// Space-separated keyword list for Win32 Resource Scripts (`.rc`).
/// Installed via the `LexCPP` lexer's `SCI_SETKEYWORDS(0, ...)` for
/// `SCE_C_WORD` (the blue "Keyword" slot).
///
/// **Single-class theme.** RC has no primitive-type vocabulary worth
/// splitting into class 1 — every keyword here is a structural,
/// declarative, or attribute word. RC is the first single-class
/// LexCPP-family theme; the rest (C / C++ / C# / Objective-C / Java)
/// install both class 0 and class 1.
///
/// **All-uppercase by convention.** Real-world `.rc` files use
/// uppercase keywords almost universally — rc.exe accepts
/// case-insensitive but Notepad++, Visual Studio's resource editor,
/// and our case-sensitive `lmCPP` factory only highlight the
/// uppercase form. A `dialog` (lowercase) declaration would render
/// uncoloured.
///
/// Eight logical categories:
///
///   1. **Resource type declarators** — `ACCELERATORS` / `ANICURSOR`
///      / `ANIICON` / `BITMAP` / `CURSOR` / `DESIGNINFO` / `DIALOG`
///      / `DIALOGEX` / `FONT` / `HTML` / `ICON` / `MENU` / `MENUEX`
///      / `MESSAGETABLE` / `PLUGPLAY` / `RCDATA` / `STRINGTABLE` /
///      `TEXTINCLUDE` / `TOOLBAR` / `TYPELIB` / `VERSIONINFO` /
///      `VXD`.
///   2. **Block delimiters** — `BEGIN` / `END`.
///   3. **Dialog control statements** — `AUTO3STATE` /
///      `AUTOCHECKBOX` / `AUTORADIOBUTTON` / `CHECKBOX` / `COMBOBOX`
///      / `CONTROL` / `CTEXT` / `DEFPUSHBUTTON` / `EDITTEXT` /
///      `GROUPBOX` / `LISTBOX` / `LTEXT` / `PUSHBOX` / `PUSHBUTTON`
///      / `RADIOBUTTON` / `RTEXT` / `SCROLLBAR` / `STATE3` /
///      `USERBUTTON` (plus `ICON` shared with category 1).
///   4. **Dialog / resource attributes** — `CAPTION` /
///      `CHARACTERISTICS` / `CLASS` / `EXSTYLE` / `LANGUAGE` /
///      `STYLE` / `VERSION` (plus `FONT` / `MENU` shared with
///      category 1).
///   5. **Menu structure + state flags** — `MENUITEM` / `POPUP` /
///      `SEPARATOR` / `CHECKED` / `GRAYED` / `HELP` / `INACTIVE` /
///      `MENUBARBREAK` / `MENUBREAK`.
///   6. **Accelerator flags** — `VIRTKEY` / `ASCII` / `NOINVERT` /
///      `ALT` / `SHIFT` (plus `CONTROL` shared with category 3).
///   7. **VERSIONINFO sub-statements** — `FILEVERSION` /
///      `PRODUCTVERSION` / `FILEFLAGSMASK` / `FILEFLAGS` / `FILEOS`
///      / `FILETYPE` / `FILESUBTYPE` / `BLOCK` / `VALUE`.
///   8. **Legacy memory attributes** (16-bit-era; rc.exe still
///      accepts them and long-lived `.rc` codebases like Wine /
///      `ReactOS` still contain them) — `DISCARDABLE` / `MOVEABLE`
///      / `FIXED` / `PURE` / `IMPURE` / `PRELOAD` / `LOADONCALL` /
///      `SHARED` / `NONSHARED`. Plus the style-expression operator
///      `NOT` and the toolbar item word `BUTTON`.
///
/// **Deliberately excluded:**
///   - **Library constants** from `windows.h` — `WS_*` / `DS_*` /
///     `BS_*` / `ES_*` / `IDOK` / `IDCANCEL` / `IDS_*` / `IDD_*` /
///     `IDC_*` / `IDM_*` and all other `#define`d symbols. They are
///     identifiers, not RC vocabulary; Lexilla routes them through
///     `SCE_C_IDENTIFIER` (uncoloured) which matches every other
///     `.rc`-aware editor.
///   - **Preprocessor directives** (`#include` / `#define` / `#ifdef`
///     / etc.) — Lexilla styles `#`-prefixed forms via
///     `SCE_C_PREPROCESSOR`.
///   - **Resource-ID-style symbols** (`IDR_MAIN_MENU` / `IDD_ABOUT`
///     / etc.) — user-defined identifiers, not RC keywords.
///   - **`USER`** — sometimes listed in informal RC references but
///     not a documented rc.exe statement keyword. Including it would
///     mis-colour any variable named `USER`. Distinct from
///     `USERBUTTON` (which IS a documented control statement).
///   - **`DLGINIT`** — internal Visual Studio resource-editor type
///     emitted into compiled `.res` output for dialog-initialisation
///     data; not a source-level `.rc` keyword.
///
/// **Accepted false-positive risk:** several short RC keywords
/// (`VALUE` / `VERSION` / `LANGUAGE` / `STYLE` / `CLASS` / `BLOCK`
/// / `NOT` / `SHARED` / `FIXED` / `PURE`) are also legal identifier
/// names that could appear in a hand-rolled `#define` block or
/// `#include`d header at the top of a `.rc` file. Notepad++ /
/// Visual Studio's resource editor accept the trade-off — the
/// keyword form dominates in real `.rc` content. Code++ follows
/// that baseline rather than under-colouring the keywords, which
/// are far more common in practice. The theme only applies to
/// `.rc` files (gated by `L_RC` in `lang_theme`), so a `style`
/// variable in a `.c` file is unaffected.
///
/// Sourced and adversarially verified across three lenses (MSDN
/// spec / production `.rc` corpora / editor baselines).
pub const RC_KEYWORDS: &str = concat!(
    "ACCELERATORS ALT ANICURSOR ANIICON ASCII AUTO3STATE AUTOCHECKBOX ",
    "AUTORADIOBUTTON BEGIN BITMAP BLOCK BUTTON CAPTION CHARACTERISTICS CHECKBOX ",
    "CHECKED CLASS COMBOBOX CONTROL CTEXT CURSOR DEFPUSHBUTTON DESIGNINFO DIALOG ",
    "DIALOGEX DISCARDABLE EDITTEXT END EXSTYLE FILEFLAGS FILEFLAGSMASK FILEOS ",
    "FILESUBTYPE FILETYPE FILEVERSION FIXED FONT GRAYED GROUPBOX HELP HTML ICON ",
    "IMPURE INACTIVE LANGUAGE LISTBOX LOADONCALL LTEXT MENU MENUBARBREAK ",
    "MENUBREAK MENUEX MENUITEM MESSAGETABLE MOVEABLE NOINVERT NONSHARED NOT ",
    "PLUGPLAY POPUP PRELOAD PRODUCTVERSION PURE PUSHBOX PUSHBUTTON RADIOBUTTON ",
    "RCDATA RTEXT SCROLLBAR SEPARATOR SHARED SHIFT STATE3 STRINGTABLE STYLE ",
    "TEXTINCLUDE TOOLBAR TYPELIB USERBUTTON VALUE VERSION VERSIONINFO VIRTKEY VXD"
);

/// Space-separated primary keyword list for C#. Installed via the
/// `LexCPP` lexer's `SCI_SETKEYWORDS(0, ...)` for `SCE_C_WORD` (the
/// blue "Keyword" slot). Covers C# 12 reserved words, contextual
/// keywords, LINQ query vocabulary, modern pattern-match operators
/// (`and`/`or`/`not`/`when`), and record-related modifiers
/// (`record`/`init`/`required`/`with`/`scoped`).
///
/// Primitive type aliases (`int`/`string`/`bool`/`nint`/`nuint`/...)
/// plus the type-related contextual keywords `var` and `dynamic`
/// live in [`CS_KEYWORDS_2`], which installs to class 1 / `SCE_C_WORD2`
/// for the steel-blue "Keyword2" slot — same blue-vs-steel-blue split
/// Notepad++ uses for C# by default.
///
/// Deliberately excluded:
///   - **Preprocessor directive names** (`define`, `region`, `pragma`,
///     `nullable`, ...): Lexilla styles `#`-prefixed directives via
///     `SCE_C_PREPROCESSOR`, independent of class 0. Including them
///     here would cause double-styling.
///   - **`args`**: not a C# keyword. It's the conventional parameter
///     name for top-level statements (the synthesised `Main(string[]
///     args)`). Colouring it would mis-render every user variable
///     named `args` (extremely common in real code) as a keyword.
///   - **`field`** (C# 13 contextual): keyword only inside property
///     accessors and `[field: ...]` attribute targets; `LexCPP` can't
///     distinguish those contexts at class-0 lookup time, so the
///     identifier sense (a `var field = ...` declaration) is the more
///     common case to honour.
///   - **`extension`** (C# 14 preview): not shipped yet and a common
///     identifier name (`var extension = path.GetExtension(...)`).
///   - **Library identifiers** (`Console`, `String` capitalised,
///     `Task`, `Math`, `IEnumerable`, ...): library types, not
///     language vocabulary.
///
/// Sourced and adversarially verified across three lenses (Microsoft
/// Learn reference / production-repo frequency / editor baselines).
pub const CS_KEYWORDS: &str = concat!(
    "abstract add alias allows and as ascending async await base break by case ",
    "catch checked class const continue default delegate descending do else ",
    "enum equals event explicit extern false file finally fixed for foreach ",
    "from get global goto group if implicit in init interface internal into is ",
    "join let lock managed nameof namespace new not notnull null on operator or ",
    "orderby out override params partial private protected public readonly ",
    "record ref remove required return scoped sealed select set sizeof ",
    "stackalloc static struct switch this throw true try typeof unchecked ",
    "unmanaged unsafe using value virtual volatile when where while with yield"
);

/// Space-separated secondary (type) keyword list for C#. Installed
/// via `SCI_SETKEYWORDS(1, ...)` for `SCE_C_WORD2` colouring. Built-in
/// primitive type aliases plus the type-inference / dynamic-typing
/// contextual keywords (`var`, `dynamic`) that every mainstream C#
/// editor visually groups with types.
pub const CS_KEYWORDS_2: &str = concat!(
    "bool byte char decimal double dynamic float int long nint nuint object ",
    "sbyte short string uint ulong ushort var void"
);

/// Space-separated primary-keyword list for Rust. `LexRust`'s keyword
/// classes 0 = primary, 1 = secondary; we install just primary at m1.
pub const RUST_KEYWORDS: &str = concat!(
    "as async await break const continue crate dyn else enum extern false fn ",
    "for if impl in let loop match mod move mut pub ref return self Self ",
    "static struct super trait true try type union unsafe use where while"
);

/// Space-separated HTML tag-name list installed via the hypertext
/// lexer's `SCI_SETKEYWORDS(0, ...)`. Without this list every tag
/// renders as `SCE_H_TAGUNKNOWN`; with it, known tags render as
/// `SCE_H_TAG` so the theme can colour real markup distinctly from
/// arbitrary identifiers a user might write inside angle brackets.
///
/// **Shared across HTML / ASP / JSP / PHP.** All four use the same
/// hypertext lexer and the same class 0 wordlist. Adding a new
/// element here lights it up across every hypertext-driven language
/// at once.
///
/// **140 tag names** covering three categories:
///
///   1. **Current WHATWG Living Standard** (~112 entries) — every
///      element in today's published HTML spec. Includes the modern
///      additions: `dialog` (HTML 5.2+), `hgroup` (removed 2013,
///      re-added 2022), `search` (2022), `slot` / `template` (Web
///      Components), `picture` / `source` / `track` (responsive
///      media), `output` / `progress` / `meter` (form widgets),
///      `details` / `summary` (disclosure), `data` (machine-readable
///      values), and the full Ruby annotation set (`ruby` / `rp` /
///      `rt`).
///   2. **Foreign-content entry points** — `svg` (SVG root) and
///      `math` (`MathML` root). The nested SVG / `MathML` element
///      vocabularies (`g` / `path` / `circle` / `mrow` / `mfrac` /
///      etc.) are deliberately NOT included — those are separate
///      lexers' territory; Code++ colours only the HTML-side entry
///      tags.
///   3. **Deprecated-but-still-supported HTML4 / Netscape-era**
///      (26 entries) — `acronym` / `applet` / `basefont` / `big` /
///      `blink` / `center` / `dir` / `font` / `frame` / `frameset` /
///      `isindex` / `keygen` / `listing` / `marquee` / `menuitem` /
///      `nobr` / `noembed` / `noframes` / `param` / `plaintext` /
///      `rb` / `rtc` / `spacer` / `strike` / `tt` / `xmp`. None
///      are in the current spec but every browser still parses them
///      and CMS-generated content / email templates / legacy
///      codebases use them. Notepad++ / VS Code / Sublime all
///      colour them. Code++ follows that baseline so maintainers of
///      old codebases see consistent highlighting instead of the
///      surprise of `SCE_H_TAGUNKNOWN`.
///
/// **Deliberately excluded:**
///   - **Hyphenated tokens** (`aria-*` / `data-*` / `accept-charset`
///     / `http-equiv` / `xml:lang`): Lexilla's wordlist matcher
///     tokenises on identifier boundaries — the hyphen / colon would
///     prevent any match.
///   - **HTML attribute names** (`class` / `id` / `href` / `src` /
///     event handlers `onclick` / etc.): the hypertext lexer's
///     class 0 is documented as "HTML elements and attributes" so
///     attributes WOULD distinguish `SCE_H_ATTRIBUTE` from
///     `SCE_H_ATTRIBUTEUNKNOWN`, but today `HYPERTEXT_STYLES` maps
///     both to the same `StyleSlot::Keyword2`. Adding ~330 attribute
///     entries here has no visible effect until a future palette
///     change splits the two slots. The same scope discipline was
///     used in the PHP commit when `SCE_HJ_*` / `SCE_HB_*` /
///     `SCE_HP_*` were deferred from `HYPERTEXT_STYLES` until those
///     embedded-language rows ship.
///   - **SGML / DTD markup** (`!DOCTYPE` / `!ENTITY` / `!ELEMENT`):
///     the lexer handles those via `SCE_H_SGML_*` (class 5),
///     independent of class 0.
///   - **CSS property names** and **JavaScript identifiers**: owned
///     by the `L_CSS` and embedded-script rows when those land.
///
/// Sourced and adversarially verified across three lenses (WHATWG /
/// W3C spec / production HTML corpora / editor baselines).
pub const HTML_KEYWORDS: &str = concat!(
    "a abbr acronym address applet area article aside audio b base basefont ",
    "bdi bdo big blink blockquote body br button canvas caption center cite ",
    "code col colgroup data datalist dd del details dfn dialog dir div dl dt ",
    "em embed fieldset figcaption figure font footer form frame frameset h1 h2 ",
    "h3 h4 h5 h6 head header hgroup hr html i iframe img input ins isindex kbd ",
    "keygen label legend li link listing main map mark marquee math menu ",
    "menuitem meta meter nav nobr noembed noframes noscript object ol optgroup ",
    "option output p param picture plaintext pre progress q rb rp rt rtc ruby s ",
    "samp script search section select slot small source spacer span strike ",
    "strong style sub summary sup svg table tbody td template textarea tfoot ",
    "th thead time title tr track tt u ul var video wbr xmp"
);

/// Space-separated Pascal keyword list for the `LexPascal` lexer.
/// Installed via `SCI_SETKEYWORDS(0, ...)` — the lexer's only
/// keyword class, descriptor "Keywords".
///
/// **All-lowercase by lexer mandate.** `LexPascal.cxx:278` calls
/// `sc.GetCurrentLowered(s, sizeof(s))` before `keywords.InList(s)`,
/// so source tokens are normalised to lowercase before lookup. The
/// wordlist MUST be all-lowercase; uppercase entries would never
/// match. Pascal source code can use any casing (`Begin` / `BEGIN`
/// / `begin` all match `begin` here) — the universal Pascal
/// convention of case-insensitive identifiers is honoured
/// transparently by the lexer.
///
/// Covers the union of three Pascal dialects:
///
///   1. **ISO Pascal (1990)** — control-flow keywords (`if` / `then`
///      / `else` / `for` / `while` / `repeat` / etc.), declarations
///      (`program` / `var` / `const` / `type` / `procedure` /
///      `function`), logical operators (`and` / `or` / `not` /
///      `div` / `mod` / `in`), structural type keywords (`array` /
///      `record` / `set` / `file` / `packed`), constants (`true` /
///      `false` / `nil`).
///   2. **Delphi / Object Pascal** — OOP (`class` / `object` /
///      `inherited` / `override` / `virtual` / `dynamic` /
///      `abstract` / `private` / `protected` / `public` /
///      `published` / `strict` / `property`), exception handling
///      (`try` / `except` / `finally` / `raise` / `on`),
///      typecasting (`is` / `as`), units / packages (`unit` /
///      `uses` / `interface` / `implementation` / `initialization`
///      / `finalization` / `library` / `package`), calling
///      conventions (`cdecl` / `stdcall` / `safecall` / `pascal` /
///      `register` / `winapi`).
///   3. **Free Pascal (FPC)** — operator overloading
///      (`operator`), generics (`generic` / `specialize`), helper
///      types (`helper`), Objective-C bridge (`objccategory` /
///      `objcclass` / `objcprotocol`), additional calling
///      conventions (`cppdecl` / `mwpascal` / `syscall` /
///      `vectorcall` / `ms_abi_*` / `sysv_abi_*`), parameter
///      modifiers (`out` / `constref`), procedure attributes
///      (`iocheck` / `nostackframe` / `saveregisters` / `softfloat`
///      / `noreturn` / `local` / `unimplemented`).
///
/// **Context-sensitive property accessors** (`index` / `name` /
/// `read` / `write` / `default` / `nodefault` / `stored` /
/// `implements` / `readonly` / `writeonly` / `add` / `remove`) are
/// included — `LexPascal.cxx:296-306` handles the suppression
/// internally (these are styled as identifiers when NOT inside a
/// `property` or `exports` declaration). The wordlist is the
/// universe; the lexer decides when to apply.
///
/// **Predefined types** (`integer` / `boolean` / `char` / `string`
/// / `byte` / `word` / `cardinal` / `real` / `extended` /
/// `pointer` / `pchar` / `ansistring` / `widestring` /
/// `unicodestring` / etc.) are included even though they are
/// technically predeclared identifiers in the `System` unit rather
/// than reserved words. Every Pascal editor — Notepad++ / Lazarus
/// IDE / RAD Studio / VS Code Pascal extension — paints them
/// keyword-blue, and matching that baseline is more important than
/// strict ISO-grammar pedantry.
///
/// **Control-flow primitives kept despite being predeclared
/// procedures** — `break` / `continue` / `exit` are System-unit
/// procedures (`break;` / `continue;` / `exit;` invoke procedures
/// rather than executing reserved-word control flow), but every
/// mainstream Pascal editor and the upstream Lexilla default
/// Pascal config paint them as keywords because users perceive
/// them semantically as control-flow. We follow that convention.
/// (Adversarial workflow verifier flagged this as a blocker for
/// strict reserved-word interpretation; the override is explicit
/// here.)
///
/// **Deliberately excluded:**
///   - **Pure RTL intrinsics** — `length` / `sizeof` / `inc` /
///     `dec` / `writeln` / `readln` / `ord` / `chr` / `pred` /
///     `succ`: standard-library functions, not language vocabulary.
///     Dialect-specific signatures. NOTE: `read` and `write` ARE
///     in the wordlist because they double as the Delphi property
///     accessor keywords; the lexer's smart-highlighting block
///     suppresses the keyword styling for both tokens when they
///     appear outside `property` declarations (see context-sensitive
///     section above), so a `WriteLn` call still renders as an
///     identifier in normal code.
///   - **Memory primitives** — `new` / `dispose` / `halt`:
///     System-unit predeclared procedures. Looking like procedure
///     calls (`new(p)` / `dispose(p)` / `halt;`), not control
///     keywords. Less universally highlighted than the
///     break/continue/exit trio.
///   - **Library class names** — `TObject` / `TStrings` / `TList`
///     / `TComponent` / etc.: VCL / LCL / RTL types, not language
///     vocabulary. The lexer styles them as `SCE_PAS_IDENTIFIER`.
///   - **Operator punctuation** (`:=` / `<` / `>` / `+` / `-` /
///     etc.): styled via `SCE_PAS_OPERATOR`, not via the wordlist.
///     Word operators (`and` / `or` / `not` / `xor` / `shl` /
///     `shr` / `div` / `mod` / `in` / `is` / `as`) ARE in the
///     wordlist because the lexer matches them through the
///     keyword path and emits `SCE_PAS_WORD`.
///
/// Sourced and adversarially verified across three lenses (ISO +
/// Delphi + FPC spec / production Pascal corpora / editor
/// baselines).
pub const PASCAL_KEYWORDS: &str = concat!(
    "absolute abstract add and ansistring array as asm assembler automated ",
    "begin boolean break byte cardinal case cdecl char class comp const ",
    "constref constructor contains continue cppdecl currency default delayed ",
    "deprecated destructor dispid dispinterface div do double downto dynamic ",
    "else end except exit experimental export exports extended external false ",
    "far file final finalization finally for forward function generic goto ",
    "helper if implementation implements in index inherited initialization ",
    "inline int64 integer interface interrupt iocheck is label library local ",
    "longint longword message mod ms_abi_cdecl ms_abi_default mwpascal name ",
    "near nil nodefault noreturn nostackframe not objccategory objcclass ",
    "objcprotocol object of olevariant on operator or out overload override ",
    "package packed pascal pchar platform pointer private procedure program ",
    "property protected public published qword raise read readonly real ",
    "record reference register reintroduce remove repeat requires ",
    "resourcestring safecall saveregisters sealed set shl shortint shr single ",
    "smallint softfloat specialize static stdcall stored strict string syscall ",
    "sysv_abi_cdecl sysv_abi_default then threadvar to true try type ",
    "unicodestring unimplemented unit unsafe until uses var varargs variant ",
    "vectorcall virtual while widestring winapi with word write writeonly xor"
);

/// Space-separated GNU Make directive list for the `LexMake` lexer.
/// Installed via `SCI_SETKEYWORDS(0, ...)` — the lexer's single
/// keyword class, descriptor "Directives".
///
/// **Single-class theme.** `LexMake` takes only one wordlist: first-
/// word-on-a-line directives. If a line's leading identifier
/// matches an entry here AND the line does not contain `:` or `=`,
/// the directive is styled as `SCE_MAKE_PREPROCESSOR`. Recipes
/// (tab-prefixed command lines), variable references (`$(VAR)`),
/// targets (identifier followed by `:`), and operators (`=` / `:=`
/// / `?=` / `+=`) are all routed syntactically — none drive
/// wordlist lookups.
///
/// **All-lowercase by convention.** GNU Make directives are
/// lowercase in source; the lexer is case-sensitive (an uppercase
/// `IFDEF` in source would NOT match against `ifdef` here).
///
/// Five categories (17 entries):
///
///   1. **Conditional** (6) — `ifdef` / `ifndef` / `ifeq` / `ifneq`
///      / `else` / `endif` (GNU Make manual §7).
///   2. **Define / undefine** (3) — `define` / `endef` (multi-line
///      definitions, §6.8) / `undefine` (GNU Make 3.82+).
///   3. **Include** (2) — `include` and `sinclude` (the
///      hyphen-free alias of `-include` for parsers that tokenise
///      identifiers without leading hyphens; the lexer's
///      directive-line gate at `LexMake.cxx:159` rejects leading
///      `-`, so `-include` would never match anyway).
///   4. **Visibility** (4) — `override` (§5.7.2) / `export` /
///      `unexport` (§5.7.4) / `private` (GNU Make 3.82+, §6.13).
///   5. **Path + dynamic-extension** (2) — `vpath` (§13.2) / `load`
///      (GNU Make 4.0+, §12.2 dynamic-object extension).
///
/// **Deliberately excluded:**
///   - **NMAKE `!`-prefixed directives** (`!IF` / `!IFDEF` /
///     `!ELSE` / etc.): the lexer styles the entire `!`-prefixed
///     line as `SCE_MAKE_PREPROCESSOR` via the `!` trigger at
///     `LexMake.cxx:155` — adding them to the wordlist would have
///     no effect.
///   - **Built-in functions** (`call` / `eval` / `foreach` /
///     `shell` / `filter` / `patsubst` / `subst` / etc.): they
///     appear inside `$(...)` and tokenise as
///     `SCE_MAKE_IDENTIFIER`, not via class 0.
///   - **Automatic variables** (`$@` / `$<` / `$^` / `$?` / `$*` /
///     `$+` / `$|` / `$%`): styled as `SCE_MAKE_IDENTIFIER` by the
///     `$(` trigger.
///   - **Special targets** (`.PHONY` / `.SUFFIXES` / `.DEFAULT` /
///     `.PRECIOUS` / etc.): appear as targets followed by `:` and
///     style as `SCE_MAKE_TARGET`.
///   - **Hyphenated `-include`**: see note 3 above.
///
/// Sourced and adversarially verified across three lenses (GNU
/// Make manual / production Makefile corpora / editor baselines).
pub const MAKEFILE_KEYWORDS: &str = concat!(
    "define else endef endif export ifdef ifeq ifndef ifneq include load ",
    "override private sinclude undefine unexport vpath"
);

/// Space-separated cmd.exe **intrinsic** keyword list for the Batch
/// lexer's wordlist 0. Installed via `SCI_SETKEYWORDS(0, ...)` against
/// `lmBatch` — the lexer matches these tokens against `SCE_BAT_WORD`
/// (style index 2).
///
/// **Case-insensitive contract.** `LexBatch` lowercases each source
/// line before wordlist comparison (`LexBatch.cxx:233` — *"All testing is
/// performed on a lower case version of the line since batch is
/// case-insensitive"*). The wordlist itself must therefore be
/// all-lowercase; uppercase tokens never match.
///
/// **Wordlist 0 vs. wordlist 1.** The split mirrors cmd.exe's own
/// dispatch model — wordlist 0 carries the tokens cmd.exe parses /
/// resolves directly, wordlist 1 carries the names cmd hands off to
/// PATH-resolved external programs. First-hit rule applies inside the
/// lexer (a token is in exactly one list); a `cd` token always styles
/// as wordlist 0 even though `cd.exe` does exist on some forks.
///
/// **Categories** (73 entries):
///
/// 1. **Control flow** (`if`/`else`/`for`/`in`/`do`/`goto`/`call`/`exit`)
///    — the cmd parser keywords. `exit` lives here because `EXIT /B` is
///    parsed by cmd, not dispatched.
/// 2. **`IF` predicates and comparison operators**
///    (`defined`/`not`/`errorlevel`/`exist` + `equ`/`neq`/`lss`/`leq`/
///    `gtr`/`geq`) — documented under `IF /?`; `IF NOT EXIST foo` lexes
///    as four tokens.
/// 3. **Core intrinsics** (`set`/`setlocal`/`endlocal`/`shift`/`echo`/
///    `rem`/`pause`/`prompt`/`title`) — everything `cmd /?` lists as
///    "built into Windows command shell".
/// 4. **Filesystem builtins** (`cd`/`chdir`/`pushd`/`popd`/`dir`/`copy`/
///    `move`/`del`/`erase`/`ren`/`rename`/`mkdir`/`md`/`rmdir`/`rd`/
///    `type`/`more`/`cls`/`mklink`) — resolved by cmd directly. Both
///    alias spellings included (`chdir`/`cd`, `mkdir`/`md`, …).
/// 5. **Environment / info** (`ver`/`vol`/`date`/`time`/`path`/`color`/
///    `assoc`/`ftype`/`label`/`help`/`print`).
/// 6. **Control-flow-adjacent** (`choice`/`start`/`break`/`verify`/
///    `loadhigh`/`lh`) — `loadhigh` and `lh` are explicitly recognised
///    by `LexBatch.cxx:360` (`InList(word, {"call","do","loadhigh","lh"})`)
///    so the lexer can apply the "next token is an external command"
///    rule; omitting them from wordlist 0 would defeat that.
/// 7. **`FOR /F` option keywords** (`tokens`/`delims`/`eol`/`skip`/
///    `usebackq`) — bare keywords inside the `"tokens=… delims=…"`
///    option string.
/// 8. **`IF CMDEXTVERSION` token** (`cmdextversion`) — recognised by
///    the `IF` parser for extended-features version checks.
/// 9. **`SETLOCAL` mode tokens** (`enabledelayedexpansion`/
///    `disabledelayedexpansion`/`enableextensions`/`disableextensions`)
///    — toggle cmd parser behavior; the visual marker for delayed
///    expansion being in scope.
///
/// **Deliberate exclusions:**
///
/// - **Switch tokens** (`/a`, `/p`, `/f`, `/d`, `/i`, `/r`, `/l`, `/b`,
///   `/wait`, `/?`, …) — command-line flags, not keywords. Adding
///   them would visually flatten the keyword/flag distinction.
/// - **`true` / `false`** — cmd.exe has no boolean type. Batch idiom
///   is `1` / `0` or `defined VAR`.
/// - **`@`** — gets its own style class (`SCE_BAT_HIDE`, index 4),
///   not a wordlist entry.
/// - **`::`** — handled by `LexBatch`'s line classifier (comment,
///   index 1), not by token lookup.
/// - **Dynamic pseudo-variables** (`%errorlevel%`/`%cd%`/`%date%`/
///   `%time%`/`%random%`/`%cmdcmdline%`/`%cmdextversion%`) — render
///   through `%VAR%` expansion under `SCE_BAT_IDENTIFIER`. The bare
///   word `errorlevel` IS in the wordlist because it's also the `IF`
///   predicate keyword; the others aren't keywords in any context.
/// - **Device names** (`nul`/`con`/`prn`/`aux`/`lpt1`-`lpt9`/`com1`-
///   `com9`) — cmd.exe doesn't lex them as keywords at command
///   position; they are filename arguments to other commands. Notepad++
///   does colour them in its default scheme, but the dispatch-model
///   philosophy here keeps wordlist 0 strictly = "tokens cmd parses".
/// - **`goto:eof`** — `:eof` is a label reference, styled `SCE_BAT_LABEL`.
///
/// Sourced and adversarially verified across three lenses (cmd.exe
/// dispatch model / Notepad++ defaults / `LexBatch` source).
pub const BATCH_KEYWORDS: &str = concat!(
    "if else for in do goto call exit defined not errorlevel exist ",
    "equ neq lss leq gtr geq set setlocal endlocal shift echo rem ",
    "pause prompt title cd chdir pushd popd dir copy move del erase ",
    "ren rename mkdir md rmdir rd type more cls ver vol date time ",
    "path color assoc ftype choice start break verify label help ",
    "print mklink loadhigh lh cmdextversion tokens delims eol skip ",
    "usebackq enabledelayedexpansion disabledelayedexpansion ",
    "enableextensions disableextensions",
);

/// Space-separated **external command** keyword list for the Batch
/// lexer's wordlist 1. Installed via `SCI_SETKEYWORDS(1, ...)` against
/// `lmBatch` — the lexer matches these tokens against
/// `SCE_BAT_COMMAND` (style index 5).
///
/// **Case-insensitive contract**, same as [`BATCH_KEYWORDS`]: tokens
/// MUST be all-lowercase. **First-hit rule:** no token may appear in
/// both wordlists; everything here is a token cmd.exe does NOT parse
/// itself — it hands the token to PATH resolution and the matched
/// `*.exe` / `*.com` / `*.bat` runs.
///
/// **Categories** (87 entries — all OS-shipped Win32 utilities the
/// average batch corpus calls by bare name):
///
/// 1. **File / archive** (`attrib`/`comp`/`compact`/`cipher`/`convert`/
///    `expand`/`fc`/`find`/`findstr`/`forfiles`/`fsutil`/`makecab`/
///    `recover`/`replace`/`sort`/`subst`/`takeown`/`tree`/`where`/
///    `xcopy`/`robocopy`/`icacls`/`cacls`).
/// 2. **Codepage / console / clipboard** (`chcp`/`clip`/`mode`).
/// 3. **System info** (`driverquery`/`hostname`/`openfiles`/`query`/
///    `systeminfo`/`whoami`/`tasklist`/`auditpol`).
/// 4. **Process control** (`taskkill`/`runas`/`sc`/`schtasks`/`at`/
///    `wmic`/`shutdown`/`logoff`/`timeout`).
/// 5. **Scripting hosts / shells** (`powershell`/`pwsh`/`cscript`/
///    `wscript`/`mshta`).
/// 6. **Installers / loaders** (`msiexec`/`rundll32`/`regsvr32`/
///    `regedit`/`reg`).
/// 7. **Network** (`arp`/`ftp`/`tftp`/`getmac`/`ipconfig`/`nbtstat`/
///    `net`/`net1`/`netsh`/`netstat`/`nslookup`/`pathping`/`ping`/
///    `route`/`telnet`/`tracert`).
/// 8. **Disk / format** (`chkdsk`/`chkntfs`/`defrag`/`diskpart`/
///    `format`/`mountvol`).
/// 9. **Servicing / image** (`dism`/`sfc`/`pnputil`/`bcdedit`/
///    `secedit`/`gpresult`/`gpupdate`/`bitsadmin`/`certutil`).
/// 10. **Event log / scheduled events** (`eventcreate`/`wevtutil`).
/// 11. **Time** (`w32tm`).
///
/// **Deliberate exclusions:**
///
/// - **cmd.exe intrinsics** (`ver`/`vol`/`label`/`exit`/`help`/`more`/
///   `print`/`mklink`/…) — live in wordlist 0. cmd dispatches them
///   directly; even though `label.exe` / `more.com` exist as files,
///   cmd's own dispatch wins.
/// - **Unix tools** (`less`/`ifconfig`/…) — not shipped with Windows;
///   the Windows equivalents (`more` internal, `ipconfig` external)
///   are covered.
/// - **Dev-toolchain binaries** (`msbuild`/`devenv`/`cl`/`link`/
///   `nmake`/`mingw32-make`) — not OS-shipped; ride along with
///   Visual Studio / MinGW installs and only resolve inside a
///   Developer Command Prompt. Styling them by default implies the
///   lexer endorses a toolchain installation; this belongs in a
///   user-customisable keyword file, not the default theme. (Also,
///   `cl` and `link` are two- and four-letter tokens that collide
///   with common user identifiers.)
/// - **MMC console snap-ins** (`devmgmt.msc`/`compmgmt.msc`/etc.) —
///   the bare word `devmgmt` is not an executable on PATH; the
///   `.msc` document is launched via the Shell. Even though Notepad++
///   sometimes includes the bare word, it doesn't resolve to anything
///   from cmd, so styling it as a command would be misleading.
/// - **Removed / deprecated binaries** (`eventquery`/`eventtriggers`)
///   — both were removed from modern Windows; the former was a `.vbs`
///   wrapper, the latter is gone entirely. Including them would
///   mis-style references to programs that no longer exist.
///
/// `regedit` IS included despite being a GUI program because batch
/// scripts routinely invoke it for silent imports (`regedit /s
/// file.reg`) — a common idiom Notepad++'s default list also covers.
/// `wmic` is included despite Windows 11 24H2 deprecation because the
/// existing batch corpus still references it heavily.
///
/// Sourced and adversarially verified across three lenses (Win32
/// utility roster / Notepad++ defaults / shipped-binary inventory).
pub const BATCH_KEYWORDS_2: &str = concat!(
    "arp at attrib auditpol bcdedit bitsadmin cacls certutil chcp ",
    "chkdsk chkntfs cipher clip comp compact convert cscript defrag ",
    "dism diskpart driverquery eventcreate expand fc find findstr ",
    "forfiles format fsutil ftp getmac gpresult gpupdate hostname ",
    "icacls ipconfig logoff makecab mode mountvol mshta msiexec ",
    "nbtstat net net1 netsh netstat nslookup openfiles pathping ping ",
    "pnputil powershell pwsh query recover reg regedit regsvr32 ",
    "replace robocopy route rundll32 runas sc schtasks secedit sfc ",
    "shutdown sort subst systeminfo takeown taskkill tasklist telnet ",
    "tftp timeout tracert tree w32tm wevtutil where whoami wmic ",
    "wscript xcopy",
);

/// Space-separated SGML / DTD keyword list for XML. Installed via
/// the `xml` lexer's `SCI_SETKEYWORDS(5, ...)` — the hypertext-family
/// lexers reserve class 5 for "SGML and DTD keywords" (the wordlist
/// descriptor in `LexHTML.cxx`). Class 5 is matched against
/// `SCE_H_SGML_COMMAND` tokens — the keyword opening a markup
/// declaration like `<!ELEMENT foo (...)>` or `<!ENTITY % bar
/// "baz">`. Casing matters: SGML/DTD keywords are conventionally
/// UPPERCASE and the lexer is case-sensitive.
///
/// **Class 0 is deliberately left empty** for XML — every XML
/// document defines its own element vocabulary via DTD or schema,
/// so there is no canonical tag list to seed class 0 with. Notepad++
/// / Visual Studio / `IntelliJ` / VS Code all ship empty class-0
/// wordlists for the XML lexer for the same reason. Adding
/// speculative entries would mis-colour arbitrary user-defined
/// element names as known tags.
///
/// **Three categories** of class-5 entries:
///
///   1. **Markup-declaration keywords** (5) — the `<!KEYWORD ...>`
///      openers: `DOCTYPE` / `ELEMENT` / `ATTLIST` / `ENTITY` /
///      `NOTATION`.
///   2. **Content-model + attribute-type keywords** (12 dual-use
///      with category 1; the installed wordlist holds each
///      identifier once — `ENTITY` and `NOTATION` are listed in
///      both categories below because they appear in both grammar
///      positions, not because they're stored twice) — used inside
///      `<!ELEMENT body>` and `<!ATTLIST body>` content: `EMPTY` /
///      `ANY` (element content models); `CDATA` / `ID` / `IDREF` /
///      `IDREFS` / `NMTOKEN` / `NMTOKENS` / `ENTITY` (also a
///      markup-decl keyword; single entry) / `ENTITIES` / `NOTATION`
///      (also a markup-decl keyword; single entry) / `NUTOKEN`
///      (SGML breadth — not strictly XML 1.0 but Lexilla's wordlist
///      is labeled "SGML and DTD keywords" and `NUTOKEN` ships in
///      Notepad++'s default wordlist).
///   3. **External identifier + conditional section keywords** (5)
///      — `PUBLIC` / `SYSTEM` (external entity references), `NDATA`
///      (notation data), `INCLUDE` / `IGNORE` (conditional section
///      markers in SGML).
///
/// **Deliberately excluded:**
///   - **Hash-prefixed special words** — `#PCDATA` / `#REQUIRED` /
///     `#IMPLIED` / `#FIXED`: the lexer styles these via
///     `SCE_H_SGML_SPECIAL` (the `#` triggers a lexer state
///     transition, not a class-5 wordlist match). Including the
///     bare identifiers here would have no effect because the
///     lexer's state machine has already routed them.
///   - **XML namespace prefixes** (`xs:` / `xsl:` / `soap:` /
///     `xsi:`): these are part of tag/attribute identifier
///     spellings, not language keywords.
///   - **Library payload vocabularies** (SOAP element names, XSD
///     element names, RSS / Atom tag names): every XML format
///     defines its own schema; the lexer can't know which one's
///     active in a given file.
pub const XML_KEYWORDS: &str = concat!(
    "ANY ATTLIST CDATA DOCTYPE ELEMENT EMPTY ENTITIES ENTITY ID IDREF IDREFS ",
    "IGNORE INCLUDE NDATA NMTOKEN NMTOKENS NOTATION NUTOKEN PUBLIC SYSTEM"
);

/// Space-separated PHP reserved-word list installed via the hypertext
/// lexer's `SCI_SETKEYWORDS(4, ...)`. The hypertext lexer reserves
/// class 4 for PHP keywords; classes 0/1/2/3 are HTML / `JavaScript`
/// / `VBScript` / Python (one class per embedded language).
///
/// Covers PHP 8.x reserved words (matching the canonical list at
/// <https://www.php.net/manual/en/reserved.keywords.php>) plus the
/// language constants (`true`/`false`/`null`) and the type
/// pseudo-keywords (`int`/`string`/...) that real PHP code reads as
/// keyword-coloured. Magic constants (`__CLASS__`/`__DIR__`/etc.) are
/// included so they pick up the same colour as `class` / `function`
/// — they're not strictly reserved words but render that way in
/// every PHP-aware editor.
///
/// **All entries must be lowercase.** The hypertext lexer's PHP
/// classifier (`classifyWordHTPHP` in `LexHTML.cxx`) calls
/// `styler.GetRangeLowered(...)` on every candidate token before the
/// `keywords.InList(s)` lookup. Class 4 storage is **not** normalised
/// (only class 0, HTML tags, gets `lowerCase = true` in the
/// `WordListSet` switch), so a literal `__CLASS__` here would never
/// match against the lexer's lowercased `__class__` query.
/// Conventional PHP magic constants are written uppercase in source
/// (`__CLASS__`, `__DIR__`), but the wordlist must store the
/// lowercased form for the lookup to succeed.
pub const PHP_KEYWORDS: &str = concat!(
    "__halt_compiler abstract and array as break callable case catch class ",
    "clone const continue declare default die do echo else elseif empty ",
    "enddeclare endfor endforeach endif endswitch endwhile enum eval exit ",
    "extends final finally fn for foreach function global goto if implements ",
    "include include_once instanceof insteadof interface isset list match ",
    "namespace new or print private protected public readonly require ",
    "require_once return static switch throw trait try unset use var while ",
    "xor yield ",
    // language constants
    "true false null ",
    // type pseudo-keywords (PHP 7+ type declarations). `array` is
    // intentionally not duplicated here — it already appears in the
    // reserved-word section above; `WordList::Set` would dedupe it
    // anyway, but listing once keeps this const honest against the
    // PHP reference.
    "void int float string bool object mixed iterable never self parent this ",
    // magic constants — stored lowercase per the docstring above.
    "__class__ __dir__ __file__ __function__ __line__ __method__ ",
    "__namespace__ __trait__"
);

/// Space-separated JavaScript reserved-word list installed via the
/// hypertext lexer's `SCI_SETKEYWORDS(1, ...)`. Class 1 of
/// `htmlWordListDesc[]` drives both `SCE_HJ_WORD` and the legacy
/// `SCE_HJ_KEYWORD` class (`LexHTML` keeps both for backward
/// compatibility — same wordlist powers both), plus their ASP
/// server-side twins `SCE_HJA_WORD` / `SCE_HJA_KEYWORD`.
///
/// **Case sensitive.** JavaScript is case-sensitive and `LexHTML`
/// does NOT lowercase JS tokens before lookup. Every entry must
/// match source exactly as written — ECMAScript convention is
/// all-lowercase for reserved words.
///
/// **Categories** (49 entries):
///
/// 1. **ES5 reserved words** — the historical core, in every JS
///    engine since 1999: `break` / `case` / `catch` / `class` /
///    `const` / `continue` / `debugger` / `default` / `delete` /
///    `do` / `else` / `enum` / `export` / `extends` / `finally` /
///    `for` / `function` / `if` / `import` / `in` / `instanceof` /
///    `new` / `return` / `super` / `switch` / `this` / `throw` /
///    `try` / `typeof` / `var` / `void` / `while` / `with` / `yield`.
/// 2. **ES2015+ block-scoped bindings** — `let` / `static` (the new
///    additions; `class` / `const` / `import` / `export` / `extends`
///    / `super` are ES2015 promotions of ES5-future-reserved words
///    already covered above).
/// 3. **ES2017+ coroutines and contextual `of`** — `async` / `await`
///    / `of`. `of` is not formally reserved but every JS-aware editor
///    highlights it as part of `for-of`.
/// 4. **Strict-mode future-reserved** — `implements` / `interface` /
///    `package` / `private` / `protected` / `public`.
/// 5. **Language literals** — `true` / `false` / `null` / `undefined`.
///    `undefined` is technically a global identifier rather than a
///    reserved word, but reassigning it is a strict-mode error and
///    every JS-aware editor treats it as keyword-coloured.
///
/// **Deliberate exclusions:**
///
/// - Global objects and host APIs (`console`, `window`, `document`,
///   `Math`, `Object`, `Array`, `JSON`, `Promise`, `Date`, `RegExp`,
///   `Error`, `NaN`, `Infinity`) — these are identifiers bound at
///   runtime, not keywords. Highlighting them would mis-colour a
///   user's local `const Math = ...` shadow.
/// - DOM methods (`getElementById`, `addEventListener`,
///   `querySelector`) — methods on host objects, not language tokens.
/// - jQuery `$` and library-specific globals.
/// - `arguments` / `eval` — special identifiers but not reserved.
/// - Contextual keywords other than `of` (`from`, `as`, `get`, `set`,
///   `target`) — not reserved; meaningful only inside specific
///   syntactic positions the lexer doesn't track.
///
/// Sourced and adversarially verified across three lenses (ECMAScript
/// 2024 spec / Notepad++ baseline / hypertext-lexer source).
pub const JAVASCRIPT_KEYWORDS: &str = concat!(
    // ES5 reserved words
    "break case catch class const continue debugger default delete do ",
    "else enum export extends finally for function if import in ",
    "instanceof new return super switch this throw try typeof var ",
    "void while with yield ",
    // ES2015+ block-scoped bindings
    "let static ",
    // ES2017+ coroutines and contextual for-of
    "async await of ",
    // Strict-mode future-reserved
    "implements interface package private protected public ",
    // Language literals
    "true false null undefined",
);

/// Space-separated `VBScript` reserved-word list installed via the
/// hypertext lexer's `SCI_SETKEYWORDS(2, ...)`. Class 2 of
/// `htmlWordListDesc[]` drives both `SCE_HB_WORD` (client-side
/// `<script language=VBScript>`) and the dominant Classic ASP case
/// `SCE_HBA_WORD` (server-side `<% %>` blocks).
///
/// **All entries must be lowercase.** The hypertext lexer's
/// `VBScript` classifier (`classifyWordHTVB` in `LexHTML.cxx`) calls
/// `styler.GetRangeLowered(...)` on every candidate token before
/// `keywords.InList(s)`. Class 2's `WordListSet` entry also sets
/// `lowerCase = true` so wordlist storage is lowercased internally —
/// but writing the source as lowercase keeps this constant honest
/// against the runtime lookup shape.
///
/// **Compound forms are NOT compound tokens.** `End If`, `End Sub`,
/// `Loop While`, `Exit For`, `On Error Resume Next`, `Option Explicit`
/// — each constituent word is looked up individually and must appear
/// in this list. The lexer just renders two adjacent keyword-styled
/// tokens; no special handling needed.
///
/// **`rem` is required, not defensive.** `VBScript`'s `Rem ...`
/// statement is a line comment — `LexHTML`'s classifier explicitly
/// tests for `rem` inside `classifyWordHTVB` and switches the
/// remainder of the line to `SCE_HB_COMMENTLINE` only if the lookup
/// succeeds. Removing `rem` from the wordlist would render
/// `Rem this is a comment` as `SCE_HB_IDENTIFIER` followed by
/// default-styled body text, NOT as a comment. Keep `rem` in.
///
/// **Categories** (133 entries):
///
/// 1. **Control flow** (`if` / `then` / `else` / `elseif` / `end` /
///    `select` / `case` / `for` / `each` / `next` / `to` / `step` /
///    `do` / `loop` / `while` / `wend` / `until` / `exit`).
/// 2. **Procedure / variable declaration** (`sub` / `function` /
///    `call` / `return` / `dim` / `const` / `redim` / `preserve` /
///    `set` / `let` / `byval` / `byref`).
/// 3. **Sentinel values and literals** (`true` / `false` / `nothing`
///    / `null` / `empty`).
/// 4. **Logical operators (real `VBScript` keywords)** — `and` /
///    `or` / `not` / `xor` / `eqv` / `imp` / `mod` / `is` / `new`.
///    Unlike C-family languages where operators are punctuation,
///    these are reserved words and tokenise as `SCE_HB_WORD`.
/// 5. **Class / property / module syntax** (`class` / `public` /
///    `private` / `property` / `get` / `friend` / `default` / `me` /
///    `with`).
/// 6. **Error handling** — `on` / `error` / `resume` / `goto`. The
///    `Resume Next` and `On Error Goto 0` forms tokenise as separate
///    words (`next` already covered above).
/// 7. **Option directive** (`option` / `explicit`).
/// 8. **Miscellaneous statements** (`stop` / `randomize` / `rem`).
/// 9. **Type-conversion and message intrinsics** (`msgbox` / `inputbox`
///    / `chr` / `asc` / `cstr` / `cint` / `clng` / `cdbl` / `cdate` /
///    `cbool` / `cbyte` / `cdec` / `ccur` / `csng`).
/// 10. **String / math / array / type intrinsics** — conservative
///     baseline drawn from Notepad++ default langs.model.xml.
/// 11. **Object / date / time intrinsics** (`createobject` /
///     `getobject` / `now` / `date` / `time` / etc.).
///
/// **Scope is `VBScript` specifically** (the ASP/WSH dialect), not
/// full VB.NET. VB.NET-only tokens (`module` / `namespace` /
/// `imports` / `inherits` / `mybase` / `mustinherit` /
/// `notinheritable` / `overrides` / `shadows` / `shared` /
/// `withevents` / `handles` / `directcast` / `trycast` / `addressof`
/// / `addhandler` / `removehandler` / `raiseevent` / `partial` /
/// `lib` / `alias` / `declare` / `structure` / `interface` /
/// `implements` / `optional` / `paramarray` / `try` / `catch` /
/// `finally` / `throw` / `continue` / `andalso` / `orelse` /
/// `gettype`) are deliberately excluded — they don't exist in
/// `VBScript` and including them would mis-colour a user identifier
/// of the same name. The `L_ASP` row scopes to `.asp` (Classic ASP)
/// only; `.aspx` (ASP.NET) is a separate language not covered here.
///
/// **Intrinsic functions are included** (the type-conversion `c*`
/// family, `msgbox` / `inputbox`, common string/math/date builtins).
/// `VBScript` has no module / import system so the runtime is
/// always available, and Notepad++'s canonical `langs.model.xml`
/// "vb" instance lists them inline with the reserved words. They
/// render as keywords in every VB-aware editor (Visual Studio, the
/// VBA IDE, `SciTE`'s `vb.properties`, Notepad++) and Code++
/// matches.
///
/// **ASP intrinsic objects are deliberately excluded** (`request` /
/// `response` / `server` / `session` / `application` /
/// `objectcontext`). They are host-provided `ActiveX` objects
/// supplied by IIS, not `VBScript` language constructs — they don't
/// exist in a `.vbs` file run under WSH. Notepad++'s default does
/// not include them either. Including them would mis-colour a
/// user's local `Dim response` variable in a non-ASP context.
///
/// Sourced and adversarially verified across three lenses
/// (`VBScript` language reference / Notepad++ baseline /
/// hypertext-lexer source).
pub const VBSCRIPT_KEYWORDS: &str = concat!(
    // control flow
    "if then else elseif end select case for each next to step ",
    "do loop while wend until exit ",
    // procedure / variable declaration
    "sub function call return dim const redim preserve set let ",
    "byval byref ",
    // sentinel values and literals
    "true false nothing null empty ",
    // logical / comparison operators (real VBScript keywords)
    "and or not xor eqv imp mod is new ",
    // class / property / module syntax
    "class public private property get friend default me with ",
    // error handling
    "on error resume goto ",
    // option directive
    "option explicit ",
    // miscellaneous statements (`rem` is required — see docstring)
    "stop randomize rem ",
    // type-conversion and message intrinsics (Notepad++ default)
    "msgbox inputbox chr asc cstr cint clng cdbl cdate cbool ",
    "cbyte cdec ccur csng ",
    // string / math / array / type intrinsics
    "len mid left right trim ltrim rtrim ucase lcase ",
    "instr instrrev replace split join space string strreverse ",
    "abs int fix sgn sqr round rnd ",
    "isarray isdate isempty isnull isnumeric isobject ",
    "typename vartype array erase lbound ubound ",
    // object / date / time intrinsics
    "createobject getobject ",
    "now date time year month day hour minute second weekday ",
    "dateadd datediff datepart dateserial datevalue timeserial ",
    "timevalue monthname weekdayname",
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_dispatches_to_known_langs() {
        assert_eq!(LangType::from_extension("c"), L_C);
        assert_eq!(LangType::from_extension("h"), L_C);
        assert_eq!(LangType::from_extension("cpp"), L_CPP);
        assert_eq!(LangType::from_extension("hpp"), L_CPP);
        assert_eq!(LangType::from_extension("rs"), L_RUST);
        // Spot-check m6's expansion.
        assert_eq!(LangType::from_extension("py"), L_PYTHON);
        assert_eq!(LangType::from_extension("js"), L_JAVASCRIPT);
        assert_eq!(LangType::from_extension("html"), L_HTML);
        assert_eq!(LangType::from_extension("yaml"), L_YAML);
        assert_eq!(LangType::from_extension("toml"), L_TOML);
        assert_eq!(LangType::from_extension("go"), L_GOLANG);
        assert_eq!(LangType::from_extension("rb"), L_RUBY);
        assert_eq!(LangType::from_extension("lua"), L_LUA);
    }

    #[test]
    fn extension_is_case_insensitive() {
        assert_eq!(LangType::from_extension("CPP"), L_CPP);
        assert_eq!(LangType::from_extension("Rs"), L_RUST);
        assert_eq!(LangType::from_extension("PY"), L_PYTHON);
    }

    #[test]
    fn unknown_extension_is_text() {
        assert_eq!(LangType::from_extension("xyzzy"), L_TEXT);
        assert_eq!(LangType::from_extension(""), L_TEXT);
    }

    #[test]
    fn from_path_uses_extension() {
        assert_eq!(LangType::from_path(Path::new("foo.cpp")), L_CPP);
        assert_eq!(LangType::from_path(Path::new("foo.rs")), L_RUST);
        assert_eq!(LangType::from_path(Path::new("script.py")), L_PYTHON);
        assert_eq!(LangType::from_path(Path::new("README")), L_TEXT);
        // `Makefile` is NOT `L_TEXT` — it's matched via the
        // filenames-pattern list (see test below). A file with no
        // extension and no filename-pattern match (like `README`)
        // still falls through to `L_TEXT`.
        assert_eq!(LangType::from_path(Path::new("nope_no_match")), L_TEXT);
    }

    /// Files identified by whole-filename pattern rather than
    /// extension: the canonical Makefile set. Pre-Phase-4.5 these
    /// silently resolved to `L_TEXT` (the bug the user hit when
    /// opening their first `Makefile`); the
    /// `LangEntry::filenames` field added in this commit fixes that.
    #[test]
    fn from_path_recognises_makefile_by_filename() {
        // Bare canonical forms.
        assert_eq!(LangType::from_path(Path::new("Makefile")), L_MAKEFILE);
        assert_eq!(LangType::from_path(Path::new("GNUmakefile")), L_MAKEFILE);
        assert_eq!(LangType::from_path(Path::new("BSDmakefile")), L_MAKEFILE);
        // Autotools inputs.
        assert_eq!(LangType::from_path(Path::new("Makefile.in")), L_MAKEFILE);
        assert_eq!(LangType::from_path(Path::new("Makefile.am")), L_MAKEFILE);
        assert_eq!(LangType::from_path(Path::new("GNUmakefile.in")), L_MAKEFILE);
        // Works through directory paths.
        assert_eq!(
            LangType::from_path(Path::new("/usr/src/foo/Makefile")),
            L_MAKEFILE
        );
        // `.mk` / `.mak` extensions still work via the extension
        // fallback — those are Makefile fragments / NMAKE files.
        assert_eq!(LangType::from_path(Path::new("rules.mk")), L_MAKEFILE);
        assert_eq!(LangType::from_path(Path::new("build.mak")), L_MAKEFILE);
    }

    /// Filename matching is case-insensitive — GNU make finds either
    /// `Makefile` or `makefile`, and on case-insensitive filesystems
    /// (Windows / macOS default) the user may have any casing.
    #[test]
    fn from_path_filename_match_is_case_insensitive() {
        assert_eq!(LangType::from_path(Path::new("makefile")), L_MAKEFILE);
        assert_eq!(LangType::from_path(Path::new("MAKEFILE")), L_MAKEFILE);
        assert_eq!(LangType::from_path(Path::new("MakeFile")), L_MAKEFILE);
        assert_eq!(LangType::from_path(Path::new("gnumakefile")), L_MAKEFILE);
        assert_eq!(LangType::from_path(Path::new("MAKEFILE.IN")), L_MAKEFILE);
    }

    /// Filename match takes priority over extension match. A file
    /// named `Makefile.in` has extension `.in` (which is not in
    /// `L_MAKEFILE.extensions`) but the FULL basename matches the
    /// filenames list — that more-specific match wins and the file
    /// resolves to `L_MAKEFILE` rather than `L_TEXT`.
    #[test]
    fn from_path_filename_match_takes_priority_over_extension() {
        assert_eq!(LangType::from_path(Path::new("Makefile.in")), L_MAKEFILE);
    }

    #[test]
    fn lexer_names_for_phase4() {
        assert_eq!(L_C.lexer_name(), Some("cpp"));
        assert_eq!(L_CPP.lexer_name(), Some("cpp"));
        assert_eq!(L_RUST.lexer_name(), Some("rust"));
        assert_eq!(L_TEXT.lexer_name(), None);
        // m6 expansion: every non-text entry resolves to a Some.
        assert_eq!(L_PYTHON.lexer_name(), Some("python"));
        assert_eq!(L_HTML.lexer_name(), Some("hypertext"));
        assert_eq!(L_JSON.lexer_name(), Some("json"));
        // L_JSON5 shares the `json` lexer with L_JSON — pin that
        // explicitly so a future table edit that loses the row or
        // misroutes the lexer name produces a test failure rather
        // than silently dropping syntax highlighting on `.json5`.
        assert_eq!(L_JSON5.lexer_name(), Some("json"));
        assert_eq!(L_YAML.lexer_name(), Some("yaml"));
        // A plugin can still set a LangType not in the table via
        // NPPM_SETBUFFERLANGTYPE — that round-trips as None.
        assert_eq!(LangType(9999).lexer_name(), None);
    }

    #[test]
    fn language_name_and_desc_for_known_langs() {
        assert_eq!(L_TEXT.language_name(), Some("Normal Text"));
        assert_eq!(L_TEXT.language_desc(), Some("Normal text file"));
        assert_eq!(L_C.language_name(), Some("C"));
        assert_eq!(L_CPP.language_name(), Some("C++"));
        assert_eq!(L_RUST.language_name(), Some("Rust"));
        assert_eq!(L_RUST.language_desc(), Some("Rust source file"));
        // m6 expansion.
        assert_eq!(L_PYTHON.language_name(), Some("Python"));
        assert_eq!(L_HTML.language_name(), Some("HTML"));
        // Unknown lang returns None — dispatch translates that into
        // an empty wide-string write per the NPPM_GETLANGUAGENAME contract.
        assert_eq!(LangType(9999).language_name(), None);
    }

    #[test]
    fn npp_ids_match_compat_header() {
        // Spot-check the boundary-value ids against the LangType_ enum
        // in plugins/nppcompat-headers/Notepad_plus_msgs.h. These
        // are enough to catch a one-off drift in the middle of the
        // table.
        assert_eq!(L_TEXT.as_npp_id(), 0);
        assert_eq!(L_CPP.as_npp_id(), 3);
        assert_eq!(L_PYTHON.as_npp_id(), 22);
        assert_eq!(L_RUST.as_npp_id(), 81);
        assert_eq!(L_EXTERNAL.as_npp_id(), 93);
        // L_JSON5 sits one past L_EXTERNAL — pin the value here so
        // the compat header's implicit enum sequencing
        // (`L_EXTERNAL = 93`, `L_JSON5` next) and this constant
        // stay aligned. Drift between the two would mean a plugin
        // compiled against the C header sees a different value
        // than the Rust dispatcher resolves.
        assert_eq!(L_JSON5.as_npp_id(), 94);
    }

    #[test]
    fn lang_table_first_entry_is_normal_text() {
        // The UI always renders "Normal Text" at the top of the menu,
        // outside the alphabetical block. Pinning index 0 here means
        // the menu builder can rely on that ordering.
        assert_eq!(LANG_TABLE[0].lang, L_TEXT);
        assert_eq!(LANG_TABLE[0].menu_label, "Normal Text");
    }

    #[test]
    fn lang_table_alphabetical_after_text() {
        // From index 1 onwards entries are alphabetical (case-insensitive)
        // by `menu_label`. The menu UI's first-letter-submenu logic
        // depends on this — adjacent same-letter rows form the groups
        // it collapses.
        for window in LANG_TABLE[1..].windows(2) {
            let a = window[0].menu_label.to_ascii_lowercase();
            let b = window[1].menu_label.to_ascii_lowercase();
            assert!(
                a < b,
                "LANG_TABLE not alphabetical: {:?} >= {:?}",
                window[0].menu_label,
                window[1].menu_label,
            );
        }
    }

    #[test]
    fn lang_table_ids_unique() {
        // No two rows share a LangType — `from_extension` and
        // `lexer_name` rely on `LangType` as a primary key. A
        // duplicate would make the second row unreachable and
        // silently lose its data.
        let mut ids: Vec<i32> = LANG_TABLE.iter().map(|e| e.lang.0).collect();
        ids.sort_unstable();
        for window in ids.windows(2) {
            assert!(
                window[0] != window[1],
                "duplicate LangType in LANG_TABLE: {}",
                window[0]
            );
        }
    }

    /// Whitespace-token membership — split, not substring. Avoids
    /// false positives like "int" matching inside "interface".
    fn contains_word(list: &str, word: &str) -> bool {
        list.split_whitespace().any(|w| w == word)
    }

    /// The LexCPP-family `*_KEYWORDS_2` split is load-bearing: words
    /// present in both class 0 and class 1 take class 0's colour
    /// (`LexCPP`'s classifier checks class 0 first). Pin that every
    /// type in `*_KEYWORDS_2` is absent from `*_KEYWORDS`, so all
    /// primitives actually pick up the `SCE_C_WORD2` steel-blue
    /// rendering. A regression that copy-pastes a type back into
    /// class 0 silently downgrades the colour without breaking any
    /// other test — this assertion catches it.
    ///
    /// Data-driven shape: iterates `*_KEYWORDS_2` directly rather
    /// than a hardcoded array. A future contributor adding a new
    /// primitive to `C_KEYWORDS_2` automatically extends the test's
    /// coverage; a future contributor accidentally re-adding the
    /// same word to `C_KEYWORDS` fails the test without needing to
    /// touch the test body.
    #[test]
    fn lexcpp_family_primitive_types_live_in_class_1_only() {
        for (kw1_list, kw1_name, kw2_list, kw2_name) in [
            (C_KEYWORDS, "C_KEYWORDS", C_KEYWORDS_2, "C_KEYWORDS_2"),
            (
                CPP_KEYWORDS,
                "CPP_KEYWORDS",
                CPP_KEYWORDS_2,
                "CPP_KEYWORDS_2",
            ),
            (CS_KEYWORDS, "CS_KEYWORDS", CS_KEYWORDS_2, "CS_KEYWORDS_2"),
            (
                OBJC_KEYWORDS,
                "OBJC_KEYWORDS",
                OBJC_KEYWORDS_2,
                "OBJC_KEYWORDS_2",
            ),
            (
                JAVA_KEYWORDS,
                "JAVA_KEYWORDS",
                JAVA_KEYWORDS_2,
                "JAVA_KEYWORDS_2",
            ),
        ] {
            for primitive in kw2_list.split_whitespace() {
                assert!(
                    !contains_word(kw1_list, primitive),
                    "{kw1_name} contains `{primitive}` (also in {kw2_name}) — \
                     class 0 masks the SCE_C_WORD2 colour"
                );
            }
        }
    }
}
