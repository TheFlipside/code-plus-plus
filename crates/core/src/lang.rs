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
        // `.sty` (LaTeX style packages), `.cls` (document class
        // files), `.ltx` (LaTeX-format-source variant), `.dtx`
        // (documented-source format) all share LaTeX grammar —
        // same `\command` / `\begin{env}` / `%` comment syntax;
        // N++ also routes the four to the LaTeX lexer.
        extensions: &["latex", "sty", "cls", "ltx", "dtx"],
        filenames: &[],
    },
    LangEntry {
        lang: L_LISP,
        menu_label: "Lisp",
        desc: "Lisp source file",
        lexer: Some("lisp"),
        extensions: &["lisp", "lsp", "el", "cl"],
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
        // Matches Notepad++'s shipped `langs.model.xml` Perl row:
        // `pl pm plx perl cgi pod psgi`. `.pod` is standalone Plain
        // Old Documentation — pure POD with no Perl code — but
        // LexPerl's POD-detection state machine handles it correctly
        // (whole file enters `SCE_PL_POD` on the first `=head1`).
        // `.cgi` is Perl CGI scripts (the historical web use case);
        // `.psgi` is Perl Web Server Gateway Interface scripts.
        extensions: &["pl", "pm", "plx", "perl", "cgi", "pod", "psgi"],
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
        extensions: &["scm", "ss", "sld", "sls"],
        filenames: &[],
    },
    LangEntry {
        lang: L_BASH,
        menu_label: "Shell",
        desc: "Shell script",
        lexer: Some("bash"),
        // `sh` / `bash` are the canonical N++ default extensions per
        // shipped `langs.model.xml`. Code++ additionally claims the
        // ksh / zsh / ash / dash dialect extensions — LexBash handles
        // their lexical surface (POSIX shell + Bash extensions) well
        // enough for syntax highlighting; the dialects' divergences
        // (associative arrays in ksh93, advanced parameter expansion
        // in zsh) tokenise gracefully. `.fish` is deliberately omitted
        // — Fish is not POSIX-compatible and deserves its own L_FISH
        // row if Lexilla ever ships a fish lexer.
        extensions: &["sh", "bash", "ksh", "zsh", "ash", "dash"],
        // Canonical shell-rc + login-script filenames. The lookup path
        // is `core::lang::resolve_by_filename` (matching the L_MAKEFILE
        // precedent for `Makefile.in`); zero startup cost. `PKGBUILD`
        // is Arch's package build script — pure Bash. `configure` is
        // the autoconf-generated bootstrap script — POSIX shell with
        // heavy `$()` / `[ ]` use.
        filenames: &[
            ".bashrc",
            ".bash_profile",
            ".bash_login",
            ".bash_logout",
            ".bash_aliases",
            ".profile",
            ".zshrc",
            ".zprofile",
            ".zlogin",
            ".zlogout",
            ".zshenv",
            ".kshrc",
            "PKGBUILD",
            "configure",
        ],
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
        // `.tcl` (canonical), `.tk` (Tk script), `.itcl` (incr Tcl
        // class definitions), `.exp` (Expect — TCL-derived, same
        // lexical surface), `.wfs` (Tcl/Tk widget framework
        // scripts). N++ ships the same set as `instre1` for the
        // TCL row in `langs.model.xml`; LexTCL handles all five
        // dialects with the same wordlist surface.
        extensions: &["tcl", "tk", "itcl", "exp", "wfs"],
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

/// Space-separated primary keyword list for Visual Basic, installed
/// via `LexVB`'s `SCI_SETKEYWORDS(0, ...)` — class 0 of
/// `vbWordListDesc[]` (`LexVB.cxx:68`). Drives `SCE_B_KEYWORD`
/// (style index 3), mapped to `StyleSlot::Keyword` (bold blue).
///
/// **All entries must be lowercase.** `LexVB.cxx:208` calls
/// `sc.GetCurrentLowered(s, ...)` on every candidate token before
/// `keywords.InList(s)`. VB source can use any casing (`If` / `IF`
/// / `if` all match) — the case-insensitive convention is honoured
/// transparently. Uppercase or mixed-case entries here would never
/// match.
///
/// **Dialect scope — VB.NET superset.** `L_VB` routes both `.vb`
/// (`VB.NET`) and `.vbs` (`VBScript`) extensions to the same Lexilla
/// lexer (`lmVB`). `VB.NET` is a strict keyword superset of
/// `VBScript` (every `VBScript` reserved word is also a `VB.NET`
/// reserved word), and `.bas` / `.cls` / `.frm` VB6 / VBA source is
/// covered transitively through the VBA-only additions (`defbool`
/// / `cvar` / `clnglng` / `clngptr` / `ptrsafe` / `lset` / `rset`
/// / `load` / `unload` / `begin` / `attribute`).
///
/// **Independence from [`VBSCRIPT_KEYWORDS`].** The
/// `VBSCRIPT_KEYWORDS` const in this file (added by the ASP
/// commit) feeds the **hypertext** lexer's class 2 for server-side
/// `VBScript` inside `<% %>` blocks — a different lexer surface
/// deliberately widened with intrinsic functions (`msgbox` /
/// `inputbox` / `chr` / etc.) for ASP. `VB_KEYWORDS` (this list)
/// feeds `LexVB`'s class 0 and follows Notepad++'s shipped
/// `<Language name="vb">` `instre1` convention of excluding those
/// library identifiers — they are not Microsoft-reserved keywords;
/// including them would mis-colour user identifiers of the same
/// name.
///
/// **Class split with `VB_KEYWORDS_2`.** A token appears in
/// exactly one wordlist (the test pins this structurally). Class 0
/// = control flow, declaration modifiers, class / module / namespace
/// syntax, error handling, type-cast operator keywords, the
/// `c<Type>` conversion-function family (which IS Microsoft-reserved,
/// unlike the string / math / date intrinsics), sentinel literals,
/// logical / comparison operator keywords, `Option` directive
/// vocabulary, LINQ contextual keywords, async / iterator
/// contextual keywords, VBA `Def<Type>` statements, VB6 form
/// `Load` / `Unload`, retained-but-unused reserved words. Class 1
/// = primitive type names + `vb<Name>` intrinsic constants from
/// `Microsoft.VisualBasic.Constants`.
///
/// This is richer than Notepad++'s shipped single `instre1` block,
/// matching the C / C++ / TypeScript precedent of splitting
/// control-flow vs type vocabulary.
///
/// **Deliberate exclusions:**
///
/// - **Primitive type names** (`integer` / `long` / `double` /
///   `string` / `boolean` / `byte` / `char` / `date` / `decimal` /
///   `object` / `sbyte` / `short` / `single` / `uinteger` / `ulong`
///   / `ushort` / `currency` / `variant`) — go to `VB_KEYWORDS_2`.
///
/// - **`empty`** — the `VBScript` `Empty` sentinel literal. Listed
///   alongside `true` / `false` / `nothing` / `null` semantically,
///   but routed to `VB_KEYWORDS_2` (class 1, Keyword2 steel-blue)
///   because it's a `VBScript`-only dialect-extension marker
///   without a `VB.NET` analogue (`VB.NET` uses `Nothing` for the
///   missing-value sentinel). This asymmetry between `null` / `nothing`
///   (class 0 Keyword) and `empty` (class 1 Keyword2) is intentional
///   — see `VB_KEYWORDS_2` docstring.
///
/// - **Standard-library intrinsics** (`msgbox` / `inputbox` / `chr`
///   / `asc` / `len` / `left` / `right` / `mid` function form /
///   `trim` / `ucase` / `lcase` / `instr` / `replace` / `split` /
///   `join` / `now` / `date` function form / `time` / `year` /
///   `createobject` / `getobject` / `abs` / `sqr` / `rnd` /
///   `isarray` / `isdate` / `isempty` / `isnull` / `isnumeric` /
///   `isobject` / `typename` / `vartype` / `lbound` / `ubound` /
///   `array`). Notepad++'s `<Language name="vb">` `instre1` block
///   does NOT list these; they are library identifiers in
///   `Microsoft.VisualBasic.dll`, not Microsoft-reserved words. The
///   `c<Type>` conversion family IS included because Microsoft does
///   list it as reserved (`cbool` through `cushort` plus VBA-only
///   `ccur` / `cvar` / `clnglng` / `clngptr`).
///
/// - **.NET framework type names** (`Form` / `Application` /
///   `Console` / `System` / `Exception`) — library identifiers, not
///   language keywords.
///
/// - **ASP intrinsic objects** (`request` / `response` / `server` /
///   `session` / `application` / `objectcontext`) — host-provided
///   `ActiveX` objects supplied by IIS, not language constructs.
///
/// - **Preprocessor directives with the `#` prefix** (`#if` /
///   `#else` / `#region` / `#const` / `#externalsource` / `#disable`
///   / `#enable`). `LexVB.cxx`'s preprocessor path styles these via
///   the dedicated `SCE_B_PREPROCESSOR` slot driven by the leading
///   `#`, not via wordlist membership. Listing them here would be
///   silently dead.
///
/// - **Punctuation operators** (`=` / `&` / `+` / `-` / `*` / `/`
///   / `\` / `^` / `<<` / `>>` and compound `<op>=` forms) —
///   tokenise as `SCE_B_OPERATOR`, not as keywords. Only the NAMED
///   operators (`and` / `or` / `not` / `xor` / `mod` / `is` /
///   `isnot` / `like` / `andalso` / `orelse` / `addressof` /
///   `gettype` / `typeof` / `directcast` / `trycast` / `ctype` /
///   `new` / `nameof`) tokenise as words and are included.
///
/// - **`vb<Type>` `VarType` return-value constants** (`vbInteger`
///   / `vbLong` / `vbString` / `vbObject` etc.) — duplicate
///   type-name spelling creates visual collision (`vbInteger` next
///   to `Integer` both rendering as Keyword2); excluded from both
///   classes.
///
/// **Special case: `rem`** is deliberately NOT in this wordlist.
/// `LexVB.cxx:212-213` hard-codes `Rem` line-comment recognition
/// before consulting any wordlist — `Rem` lines style as
/// `SCE_B_COMMENT` regardless of whether `rem` is in class 0.
/// Including it here would be silently dead.
///
/// Sourced and adversarially verified across three lenses
/// (Microsoft Learn "Keywords (Visual Basic)" canonical
/// reserved-word table / Notepad++ `langs.model.xml`
/// `<Language name="vb">` `instre1` / `LexVB.cxx:215-222` wordlist
/// dispatch). Completeness verifier flagged 5 omissions
/// (`ascending` / `descending` for LINQ sorts, `off` / `infer` for
/// `Option` directives, `getxmlnamespace` for XML literals) — all
/// added before commit.
pub const VB_KEYWORDS: &str = concat!(
    // control flow
    "if then else elseif end select case for each next to step ",
    "while wend do loop until continue exit goto return resume on ",
    // procedure / variable declaration and modifiers
    "sub function dim const static shared shadows overloads overrides ",
    "overridable mustoverride notoverridable mustinherit notinheritable ",
    "partial lib alias declare property get let set withevents handles ",
    "readonly writeonly default paramarray byval byref optional ",
    "redim preserve erase ",
    // class / module / namespace syntax
    "class module namespace interface structure enum delegate event ",
    "raiseevent addhandler removehandler operator implements inherits ",
    "imports public private protected friend global of as in out ",
    "narrowing widening ",
    // self-reference
    "me mybase myclass ",
    // error handling (`error` is the legacy `On Error` keyword)
    "try catch finally throw error ",
    // type / cast operator keywords (real reserved words, not library)
    "ctype directcast trycast addressof gettype typeof nameof ",
    // XML-literal namespace lookup (VB.NET-unique reserved keyword)
    "getxmlnamespace ",
    // logical / comparison operator keywords. `eqv` / `imp` are
    // VBScript / VB6 / VBA-only — removed from `VB.NET` proper, where
    // using them as identifiers raises a compile error but they are
    // NOT reserved words. Kept here because `L_VB` covers the whole
    // VB family (`VB.NET` superset on the `VB.NET` axis, but with
    // legacy-dialect operators included transitively for `.bas` /
    // `.cls` / `.vbs` files that route through the same lexer).
    // Impact on `VB.NET`-only files: harmless — `eqv` / `imp` as
    // identifiers are extremely rare and the colour wouldn't be
    // semantically meaningful anyway.
    "and andalso or orelse not xor eqv imp mod is isnot like new ",
    // type-conversion function keywords (Microsoft-reserved `c<Type>`
    // family; `ccur` / `cvar` / `clnglng` / `clngptr` are VBA-only)
    "cbool cbyte cchar cdate cdbl cdec cint clng cobj csbyte cshort ",
    "csng cstr cuint culng cushort ccur cvar clnglng clngptr ",
    // sentinel literals (`empty` deliberately omitted — routed to
    // `VB_KEYWORDS_2` alongside other VBScript dialect markers)
    "true false nothing null ",
    // `Option` directive vocabulary
    "option explicit strict compare binary text infer off ",
    // `Declare` / assembly-attribute modifiers (separate from `Option`
    // despite tokenising as plain words)
    "unicode ansi assembly ",
    // LINQ / query contextual keywords (`select` / `on` / `let`
    // already listed above — single-class, first-occurrence wins)
    "from where group by into join equals aggregate distinct ",
    "order skip take ascending descending ",
    // async / iterator / event / scope contextual keywords
    "async await yield iterator custom when using synclock with ",
    // misc statements
    "call stop randomize debug print ",
    // VB6 / VBA legacy statements still in widespread use (`mid` is
    // the assignment-statement form; function form is library and
    // excluded)
    "lset rset mid load unload begin attribute ",
    // VBA `Def<Type>` default-type-by-prefix statements
    "defbool defbyte defcur defdate defdbl defdec defint deflng ",
    "deflnglng deflngptr defobj defsng defstr defvar ",
    // VBA 64-bit declaration modifier (Office 2010+)
    "ptrsafe ",
    // retained-but-unused per Microsoft (still tokenise as reserved)
    "gosub endif",
);

/// Space-separated type / intrinsic-constant list for Visual Basic,
/// installed via `LexVB`'s `SCI_SETKEYWORDS(1, ...)` — class 1 of
/// `vbWordListDesc[]`. Drives `SCE_B_KEYWORD2` (style index 10),
/// mapped to `StyleSlot::Keyword2` (steel blue) in `VB_STYLES`.
///
/// **All entries lowercase**, same case-insensitive contract as
/// [`VB_KEYWORDS`].
///
/// **No overlap with class 0.** Verified structurally by the
/// `vb_uses_lexvb_two_class_theme` test's `HashSet` intersection
/// check.
///
/// **Categories** (53 entries):
///
/// - **VB.NET primitive types** (16) — `boolean` / `byte` / `char`
///   / `date` / `decimal` / `double` / `integer` / `long` /
///   `object` / `sbyte` / `short` / `single` / `string` / `uinteger`
///   / `ulong` / `ushort`.
///
/// - **VB Classic / `VBScript` / VBA dialect-extension types and
///   literals** (3) — `currency`, `variant`, `empty`. (`empty` is
///   the `VBScript` `Empty` sentinel literal; coloured as Keyword2
///   here since it's a dialect-extension marker rather than a
///   primary control-flow word, and `VB.NET` has no `Empty` so the
///   class-0 vs class-1 split favours class 1 for the `.vb`
///   majority case.)
///
/// - **Text / line-ending intrinsic constants** (11) from
///   `Microsoft.VisualBasic.Constants` — `vbcr` / `vbcrlf` /
///   `vbformfeed` / `vblf` / `vbnewline` / `vbnull` / `vbnullchar`
///   / `vbnullstring` / `vbtab` / `vbverticaltab` / `vbback`. The
///   most heavily-typed identifiers in real VB code after the
///   primitive types themselves; every string concatenation
///   involves at least one.
///
/// - **`MsgBox` button-group constants** (6) — `vbokonly` /
///   `vbokcancel` / `vbabortretryignore` / `vbyesnocancel` /
///   `vbyesno` / `vbretrycancel`.
///
/// - **`MsgBox` icon constants** (4) — `vbcritical` / `vbquestion`
///   / `vbexclamation` / `vbinformation`.
///
/// - **`MsgBox` default-button + modality constants** (6) —
///   `vbdefaultbutton1` through `vbdefaultbutton4` /
///   `vbapplicationmodal` / `vbsystemmodal`.
///
/// - **`MsgBox` return-value constants** (7) — `vbok` / `vbcancel`
///   / `vbabort` / `vbretry` / `vbignore` / `vbyes` / `vbno`.
///
/// `MsgBox "X", vbCritical Or vbOKCancel` is the single most
/// common idiom in legacy VB6 / VBA and remains common in
/// `VB.NET`; covering the full vocabulary so users see consistent
/// highlighting across the whole `MsgBox` expression.
///
/// **Deliberate exclusions** (Notepad++ ships some; trimmed as
/// dead vocabulary):
///
/// - **Colour constants** (`vbblack` / `vbblue` / `vbcyan` /
///   `vbgreen` / `vbmagenta` / `vbred` / `vbwhite` / `vbyellow`)
///   — VB6 forms-only; modern .NET uses `Color.FromArgb`.
/// - **`FileAttribute`** (`vbnormal` / `vbhidden` / `vbreadonly`
///   / `vbsystem` / `vbvolume` / `vbdirectory` / `vbarchive` /
///   `vbalias`) — niche; `My.Computer.FileSystem` is the modern
///   equivalent.
/// - **`TriState`** (`vbtrue` / `vbfalse` / `vbusedefault`) —
///   overlaps with class 0 `true` / `false` and confuses the eye.
/// - **`CompareMethod`** (`vbbinarycompare` / `vbtextcompare` /
///   `vbdatabasecompare`) — single-site, only inside `Option
///   Compare`.
/// - **`VarType` return values** (`vbinteger` / `vblong` /
///   `vbstring` / `vbobject` / `vbarray` etc.) — duplicate
///   type-name spelling creates visual collision.
/// - **`DateFirstDayOfWeek` / `DateFirstWeekOfYear`** families —
///   locale plumbing, never seen in app code.
/// - **`CallType`** (`vbmethod` / `vbget` / `vblet` / `vbset`) —
///   reflection vocabulary, single-site.
/// - **CLR type aliases** (`int32` / `int64` / `uint32` /
///   `intptr`) — BCL type names, not VB keywords; VB source uses
///   `Integer` / `Long` / `UInteger` instead.
/// - **`DateTime` field names** (`year` / `month` / `day` / `hour`
///   / `minute` / `second`) — properties, not type names.
///
/// Sourced and adversarially verified against Microsoft Learn
/// `Microsoft.VisualBasic.Constants` reference, Notepad++
/// `langs.model.xml` `instre2`, and `LexVB.cxx:215-222`.
pub const VB_KEYWORDS_2: &str = concat!(
    // VB.NET primitive types
    "boolean byte char date decimal double integer long object sbyte ",
    "short single string uinteger ulong ushort ",
    // VB Classic / VBScript / VBA dialect-only types + literal
    "currency variant empty ",
    // Text / line-ending intrinsic constants
    "vbcr vbcrlf vbformfeed vblf vbnewline vbnull vbnullchar vbnullstring ",
    "vbtab vbverticaltab vbback ",
    // MsgBox button group constants
    "vbokonly vbokcancel vbabortretryignore vbyesnocancel vbyesno ",
    "vbretrycancel ",
    // MsgBox icon constants
    "vbcritical vbquestion vbexclamation vbinformation ",
    // MsgBox default-button + modality constants
    "vbdefaultbutton1 vbdefaultbutton2 vbdefaultbutton3 vbdefaultbutton4 ",
    "vbapplicationmodal vbsystemmodal ",
    // MsgBox return-value constants
    "vbok vbcancel vbabort vbretry vbignore vbyes vbno",
);

/// Space-separated SQL reserved-word list installed via `LexSQL`'s
/// `SCI_SETKEYWORDS(0, ...)` — class 0 of `sqlWordListDesc[]`. Drives
/// `SCE_SQL_WORD` (primary keyword bold blue).
///
/// **All entries must be lowercase.** `LexSQL.cxx:786` calls
/// `MakeLowerCase(styler[i+j])` on every candidate token before
/// `keywords.InList(s)`. SQL source can use any casing (`SELECT` /
/// `Select` / `select` all match `select`) — the case-insensitive
/// convention is honoured transparently, but uppercase entries here
/// would never match.
///
/// **Class split with `SQL_KEYWORDS_2`.** A token appears in exactly
/// one wordlist. The split mirrors Notepad++'s shipped
/// `langs.model.xml`:
///
///   * **Class 0 (this list)** — statement-level reserved words: DML
///     verbs (`select` / `insert` / `update`), DDL verbs (`create` /
///     `alter` / `drop`), DCL (`grant` / `revoke` / `commit`), clause
///     keywords (`from` / `where` / `join`), control flow (`if` /
///     `loop` / `case`), set ops (`union` / `intersect`), literals
///     (`null` / `true` / `false`), and procedural vocabulary from
///     T-SQL / PL/SQL / PL/pgSQL. The structural anchors a SQL reader
///     scans for.
///   * **Class 1 (`SQL_KEYWORDS_2`)** — built-in type names (`int` /
///     `varchar` / `timestamp`) and built-in functions (`count` /
///     `coalesce` / `cast` / `extract`).
///
/// **Window-frame vocabulary** (`current` / `following` / `groups` /
/// `nulls` / `preceding` / `unbounded` / `window`) lives in class 0:
/// `ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` reads as
/// structural keyword syntax. The window FUNCTIONS themselves
/// (`row_number` / `rank` / `lag` / `lead`) live in class 1 — they're
/// builtin functions, not clause keywords.
///
/// **Dialect scope.** ANSI SQL:2016 baseline plus the four major
/// dialects — `PostgreSQL`, MySQL/MariaDB, Microsoft SQL Server
/// (T-SQL), and Oracle (PL/SQL). Hierarchical-query (`connect by` /
/// `prior` / `level` / `rownum`), `merge`, `pivot` / `unpivot`,
/// `returning`, `ilike`, `lateral`, `forall` — all covered.
///
/// **Deliberate exclusions:**
///
/// - Cloud-warehouse extensions (`Snowflake` `qualify` /
///   `match_recognize`, `BigQuery` `safe.`, Redshift / `DuckDB`
///   dialect-specific vocabulary). Add per project need.
/// - Vendor schema identifiers (`sys` / `information_schema` /
///   `pg_catalog` / `dbo` / `master` / `mysql` /
///   `performance_schema`) — these are identifiers, not keywords;
///   including would mis-style legitimate user references.
/// - Optimiser hint contents (Oracle `/*+ ... */` body, T-SQL
///   `OPTION (HASH JOIN)` inner words like `hash` / `recompile`) —
///   too dialect-specific and overlaps too aggressively with common
///   identifier names.
/// - Punctuation / hyphenated forms — `LexSQL.cxx`'s `iswordchar`
///   treats `-` as an operator, so a hypothetical wordlist entry
///   like `end-exec` tokenises as three separate tokens (`end`, `-`,
///   `exec`) and never matches as one word.
///
/// Sourced and adversarially verified across three lenses (ANSI
/// SQL:2016 / Notepad++ baseline / `LexSQL.cxx` source). The
/// completeness verifier flagged window-frame vocabulary and window
/// function names as critical gaps — all added before commit.
pub const SQL_KEYWORDS: &str = concat!(
    // ANSI / dialect statement-level keywords (alphabetical)
    "absolute action add after alias all allocate alter analyze and any are as ",
    "asc asensitive assertion asymmetric at atomic authorization autoincrement ",
    "auto_increment backup before begin between both breadth by call called ",
    "cascade cascaded case catalog catch change charset check checkpoint class ",
    "close cluster clustered collate collation column columns comment commit ",
    "completion compute condition connect connection constraint constraints ",
    "constructor contains continue corresponding create cross cube current ",
    "cursor cycle data database databases day deallocate declare default ",
    "deferrable deferred delete deny depth deref desc describe descriptor ",
    "destroy destructor deterministic diagnostics dictionary disconnect ",
    "distinct do domain drop dual dynamic each else elseif elsif end equals ",
    "errlvl escape every except exception exclusive exec execute exists exit ",
    "explain external false fetch fields file fillfactor first following for ",
    "foreign forall found free freetext from full function general get global go ",
    "goto grant group grouping groups handler having hold host hour identified ",
    "identity if ignore ilike immediate in include index indicator initialize ",
    "initially inner inout input insensitive insert instead intersect into is ",
    "isolation iterate join key keys kill language large last lateral leading ",
    "leave left less level like limit limited local locator lock login loop map ",
    "master match materialized merge method minus minute modifies modify module ",
    "month names national natural new next no nocheck nonclustered none not null ",
    "nulls object of off offline offset offsets old on online only open ",
    "openquery openrowset operation operator option or order ordinality others ",
    "out outer output over overlaps overriding pad package parameter parameters ",
    "partial partition percent perform pivot plan postfix preceding prefix ",
    "preorder prepare preserve primary print prior privileges procedure proc ",
    "public raise raiserror read readtext reads reconfigure record recursive ",
    "references referencing refcursor relative release rename replace ",
    "replication restore restrict result return returning returns revert revoke ",
    "right role rollback rollup routine row rownum rows rowcount rule save ",
    "savepoint schema schemas scope scroll search second section securityaudit ",
    "select sensitive session set setof sets setuser share show shutdown size ",
    "some specific specifictype sql sqlcode sqlerror sqlexception sqlstate ",
    "sqlwarning start state statement static statistics structure symmetric ",
    "synonym sysname system table tables tablesample temporary terminate ",
    "textsize then throw timezone_hour timezone_minute to top trailing tran ",
    "transaction trigger true truncate try tsequal type uescape unbounded under ",
    "undo union unique unknown unnest unpivot unsigned until update updatetext ",
    "usage use using value values view waitfor when whenever where while window ",
    "with within without work write writetext xor zerofill zone",
);

/// Space-separated SQL **types and built-in functions** list
/// installed via `LexSQL`'s `SCI_SETKEYWORDS(1, ...)` — class 1 of
/// `sqlWordListDesc[]`. Drives `SCE_SQL_WORD2` (Keyword2 steel blue).
///
/// **All entries must be lowercase**, same case-insensitive contract
/// as [`SQL_KEYWORDS`].
///
/// **No overlap with class 0.** `LexSQL`'s wordlist matching is
/// first-hit (the lexer checks classes in registration order); having
/// a token in both lists either wastes bytes or produces
/// inconsistent rendering depending on Lexilla version. Every token
/// here is verified absent from `SQL_KEYWORDS`.
///
/// **Categories** (organised: built-in types, then built-in
/// functions; alphabetical within each).
///
/// **Type names** cover ANSI standard plus the four major dialects:
///
/// - ANSI: `int` / `integer` / `smallint` / `bigint` / `numeric` /
///   `decimal` / `dec` / `float` / `real` / `double` / `precision` /
///   `char` / `character` / `varchar` / `text` / `clob` / `blob` /
///   `date` / `time` / `timestamp` / `interval` / `boolean` / `bool`
///   / `binary` / `varbinary` / `varying`.
/// - `PostgreSQL`: `serial` / `bigserial` / `smallserial` / `uuid` /
///   `json` / `jsonb` / `bytea` / `money` / `cidr` / `inet` /
///   `macaddr` / `tsvector` / `tsquery` / `citext` / `hstore` /
///   `point` / `line` / `lseg` / `box` / `path` / `polygon` /
///   `circle` / `range` / `int4range` / `int8range` / `numrange` /
///   `tsrange` / `tstzrange` / `daterange`.
/// - `MySQL`: `tinyint` / `mediumint` / `tinytext` / `mediumtext` /
///   `longtext` / `tinyblob` / `mediumblob` / `longblob` / `year` /
///   `bit`. (`MySQL`'s `SET` column type clashes with the SQL `SET`
///   statement — `set` lives in class 0 / `SQL_KEYWORDS` because the
///   statement form is overwhelmingly more common; `MySQL` `SET`
///   columns render as Keyword bold rather than Keyword2 steel-blue,
///   an acceptable v1 trade-off.)
/// - SQL Server: `ntext` / `nchar` / `nvarchar` / `image` /
///   `datetime` / `datetime2` / `datetimeoffset` / `smalldatetime` /
///   `hierarchyid` / `geometry` / `geography` / `xml` / `sql_variant`
///   / `uniqueidentifier`.
/// - Oracle: `number` / `varchar2` / `nvarchar2` / `raw` / `long` /
///   `nclob` / `bfile` / `rowid` / `urowid` / `ref`.
/// - PG aliases: `serial4` / `serial8` / `int2` / `int4` / `int8` /
///   `float4` / `float8`.
///
/// **Built-in functions** — Notepad++ ships these in WORD2 because
/// they visually scan as language-level constructs in a SELECT (`COUNT(*)`
/// / `COALESCE(x, y)` / `TO_CHAR(d)`) and deserve the steel-blue
/// Keyword2 treatment distinct from user-defined identifier names:
///
/// - **ANSI niladic functions** (parenless): `current_date` /
///   `current_time` / `current_timestamp` / `current_user` /
///   `session_user` / `current_role` / `current_database` /
///   `current_schema` / `system_user` / `user` / `localtime` /
///   `localtimestamp`.
/// - **ANSI function-keywords** (parse with their own syntax):
///   `cast` / `extract` / `position` / `substring` / `trim` /
///   `convert` / `coalesce` / `nullif` / `greatest` / `least`.
/// - **Aggregates**: `count` / `sum` / `avg` / `min` / `max` /
///   `stddev` / `stddev_pop` / `stddev_samp` / `variance` /
///   `var_pop` / `var_samp` / `listagg` / `string_agg` / `array_agg`
///   / `json_agg` / `jsonb_agg`.
/// - **Window functions**: `row_number` / `rank` / `dense_rank` /
///   `ntile` / `lag` / `lead` / `first_value` / `last_value` /
///   `nth_value` / `percent_rank` / `cume_dist`.
/// - **String** (`trim` / `position` / `substring` are listed under
///   ANSI function-keywords above; not duplicated here): `length` /
///   `char_length` / `character_length` / `octet_length` / `lower` /
///   `upper` / `initcap` / `ltrim` / `rtrim` / `substr` / `replace`
///   / `concat` / `lpad` / `rpad` / `repeat` / `reverse` / `ascii` /
///   `chr` / `hex` / `unhex`.
/// - **Math**: `abs` / `acos` / `asin` / `atan` / `atan2` /
///   `ceil` / `ceiling` / `cos` / `exp` / `floor` / `log` / `mod` /
///   `power` / `round` / `sign` / `sin` / `sqrt` / `tan` / `trunc`.
/// - **Date / time**: `age` / `date_part` / `date_trunc` /
///   `dateadd` / `datediff` / `datepart` / `getdate` / `getutcdate` /
///   `now` / `sysdate` / `systimestamp` / `to_char` / `to_date` /
///   `to_number` / `to_timestamp`.
/// - **Null handling**: `nvl` / `nvl2` / `isnull` / `ifnull` /
///   `iif` / `decode`.
/// - **Hash**: `md5` / `sha1` / `sha2`.
/// - **Regex**: `regexp_replace` / `regexp_like` / `regexp_substr`
///   / `regexp_count` (Oracle / `PostgreSQL`).
/// - **Misc**: `format` / `version` / `translate` / `treat`.
///
/// Sourced and adversarially verified across three lenses (ANSI
/// SQL:2016 / Notepad++ baseline / `LexSQL.cxx` source). Completeness
/// verifier flagged the entire window-function category as missing
/// from the initial synthesis — all 11 ranking / offset / value
/// functions added before commit.
pub const SQL_KEYWORDS_2: &str = concat!(
    // Built-in types
    "bigint bigserial binary bit blob bool boolean box bytea bfile char ",
    "character cidr circle citext clob date datetime datetime2 datetimeoffset ",
    "daterange dec decimal double float float4 float8 geography geometry ",
    "hierarchyid hstore image inet int int2 int4 int4range int8 int8range ",
    "integer interval json jsonb line long longblob longtext lseg macaddr ",
    "mediumblob mediumint mediumtext money nchar nclob ntext number numeric ",
    "numrange nvarchar nvarchar2 path point polygon precision range raw real ",
    "ref rowid serial serial4 serial8 smalldatetime smallint smallserial ",
    "sql_variant text time timestamp tinyblob tinyint tinytext tsquery tsrange ",
    "tstzrange tsvector uniqueidentifier urowid uuid varbinary varchar varchar2 ",
    "varying xml year ",
    // ANSI niladic functions
    "current_database current_date current_role current_schema current_time ",
    "current_timestamp current_user localtime localtimestamp session_user ",
    "system_user user ",
    // ANSI function-keywords + conversions
    "cast coalesce convert extract greatest least nullif position substring ",
    "trim ",
    // Aggregates + statistical
    "array_agg avg count json_agg jsonb_agg listagg max min stddev stddev_pop ",
    "stddev_samp string_agg sum var_pop var_samp variance ",
    // Window functions
    "cume_dist dense_rank first_value lag last_value lead nth_value ntile ",
    "percent_rank rank row_number ",
    // String functions
    "ascii char_length character_length chr concat hex initcap length lower ",
    "lpad ltrim octet_length repeat reverse rpad rtrim substr unhex upper ",
    // Math functions
    "abs acos asin atan atan2 ceil ceiling cos exp floor log mod power round ",
    "sign sin sqrt tan trunc ",
    // Date / time functions
    "age date_part date_trunc dateadd datediff datepart getdate getutcdate now ",
    "sysdate systimestamp to_char to_date to_number to_timestamp ",
    // Null handling
    "decode ifnull iif isnull nvl nvl2 ",
    // Hash + regex + misc
    "md5 regexp_count regexp_like regexp_replace regexp_substr sha1 sha2 ",
    "format translate treat version",
);

/// Space-separated CSS1 property names installed via `LexCSS`'s
/// `SCI_SETKEYWORDS(0, ...)` — class 0 of `cssWordListDesc[]`. Drives
/// `SCE_CSS_IDENTIFIER` (the first hit in `LexCSS`'s four-way property-
/// name cascade, mapped to Keyword bold).
///
/// **All entries must be lowercase.** `LexCSS.cxx:419` calls
/// `sc.GetCurrentLowered(s, ...)` on every candidate token before
/// `WordList::InList`. CSS source can use any casing (`COLOR` /
/// `Color` / `color` all match) but uppercase wordlist entries here
/// would never match. Same shape contract as the SQL / Batch / VB
/// wordlists.
///
/// **Four-way IDENTIFIER cascade with [`CSS_PROPERTIES_CSS2`] /
/// [`CSS_PROPERTIES_CSS3`] and (future) extension wordlist.**
/// `LexCSS.cxx:425-438` consults classes 0 / 2 / 3 / 5 in priority
/// order — a token appears in exactly one class. This list covers the
/// canonical W3C CSS Level 1 (1996) property set: 5 box-model
/// shorthands + 4 background longhands, the 6 border shorthand /
/// width / colour / style longhands across all four edges, the 7
/// font-* longhands, the 4 list-style longhands, the 4 margin
/// longhands, the 4 padding longhands, the 4 text-* longhands, and
/// the dimension + layout primitives (`width` / `height` /
/// `display` / `float` / `clear` / `color` / `line-height` /
/// `letter-spacing` / `word-spacing` / `white-space` /
/// `vertical-align`). Roughly the "first-language" CSS subset that
/// every browser has supported since 1996.
///
/// Sourced from the W3C CSS 1 Recommendation, cross-checked against
/// Notepad++'s shipped `langs.model.xml` and the Lexilla
/// `LexCSS.cxx` cascade logic.
pub const CSS_PROPERTIES_CSS1: &str = concat!(
    // Background
    "background background-attachment background-color background-image ",
    "background-position background-repeat ",
    // Border (shorthand + per-edge + width / style / colour)
    "border border-bottom border-bottom-width border-color border-left ",
    "border-left-width border-right border-right-width border-style ",
    "border-top border-top-width border-width ",
    // Layout primitives
    "clear color display float ",
    // Font
    "font font-family font-size font-style font-variant font-weight ",
    // Dimensions
    "height width ",
    // Text + spacing
    "letter-spacing line-height ",
    // List
    "list-style list-style-image list-style-position list-style-type ",
    // Margin / padding
    "margin margin-bottom margin-left margin-right margin-top ",
    "padding padding-bottom padding-left padding-right padding-top ",
    // Text alignment / decoration
    "text-align text-decoration text-indent text-transform ",
    // Misc layout
    "vertical-align white-space word-spacing",
);

/// Space-separated CSS pseudo-class names installed via `LexCSS`'s
/// `SCI_SETKEYWORDS(1, ...)` — class 1 of `cssWordListDesc[]`. Drives
/// `SCE_CSS_PSEUDOCLASS` (mapped to Keyword2). Stored WITHOUT leading
/// colons — the lexer's `:` state-machine entry (`LexCSS.cxx:251-262`)
/// already routes post-colon tokens to PSEUDOCLASS state, then the
/// wordlist sweep matches the bare identifier on word-boundary.
///
/// **All entries must be lowercase.** Same `GetCurrentLowered`
/// contract as [`CSS_PROPERTIES_CSS1`].
///
/// **Legitimate state-disambiguated cross-namespace overlap.** `left`
/// and `right` appear here (paged-media pseudo-classes `:left` /
/// `:right`) AND in [`CSS_PROPERTIES_CSS2`] (positional properties).
/// Lexilla disambiguates by lexer state — class 1 lookup only fires
/// post-`:`, class 2 lookup only fires post-`{` in the property-name
/// position — so the same token in both lists is the correct
/// representation, not a duplicate to remove.
///
/// Covers Selectors Level 3 + Level 4: structural (`first-child` /
/// `nth-child` / `is` / `where` / `has` / `not`), state
/// (`hover` / `focus` / `active` / `visited` / `checked` /
/// `disabled` / `focus-visible` / `focus-within` /
/// `placeholder-shown`), form-validation
/// (`valid` / `invalid` / `in-range` / `required` / `optional` /
/// `user-valid` / `user-invalid`), input-state (`read-only` /
/// `read-write` / `autofill`), media-element (`playing` / `paused` /
/// `picture-in-picture` / `fullscreen`), tree-context (`root` /
/// `empty` / `scope` / `target` / `target-within`), and structural
/// position (`last-child` / `only-of-type` / `nth-of-type` /
/// paged-media `left` / `right` / `first`).
///
/// Sourced from W3C CSS Selectors Level 4 spec, cross-checked against
/// MDN's pseudo-class index and the Lexilla source.
pub const CSS_PSEUDO_CLASSES: &str = concat!(
    "active any-link autofill blank checked current ",
    "default defined dir disabled empty enabled ",
    "first first-child first-of-type ",
    "focus focus-visible focus-within fullscreen future ",
    "has host host-context hover ",
    "in-range indeterminate invalid is ",
    "lang last-child last-of-type left link local-link ",
    "not ",
    "nth-child nth-col nth-last-child nth-last-col nth-last-of-type nth-of-type ",
    "only-child only-of-type optional out-of-range ",
    "past paused picture-in-picture placeholder-shown playing ",
    "read-only read-write required right root ",
    "scope ",
    "target target-within ",
    "user-invalid user-valid ",
    "valid visited where",
);

/// Space-separated CSS2 property names installed via `LexCSS`'s
/// `SCI_SETKEYWORDS(2, ...)` — class 2 of `cssWordListDesc[]`. Drives
/// `SCE_CSS_IDENTIFIER2` (the second hit in the four-way property-
/// name cascade, mapped to Keyword bold — visually indistinguishable
/// from [`CSS_PROPERTIES_CSS1`] / [`CSS_PROPERTIES_CSS3`] by design).
///
/// **All entries must be lowercase** (same `GetCurrentLowered`
/// contract).
///
/// **Cascade extension of [`CSS_PROPERTIES_CSS1`].** Properties in
/// this list are CSS2 / CSS2.1 additions that are NOT in CSS1.
/// `LexCSS.cxx:431` falls through to class 2 only when class 0
/// doesn't match. No token appears in both lists.
///
/// Covers CSS2 / CSS2.1 additions: positioning (`position` / `top` /
/// `bottom` / `left` / `right` / `z-index`), display extensions
/// (`overflow` / `visibility` / `clip`), the aural / speech family
/// (`azimuth` / `cue` / `cue-after` / `cue-before` / `elevation` /
/// `pause` / `pitch` / `play-during` / `richness` / `speak` /
/// `speech-rate` / `stress` / `voice-family` / `volume` and
/// friends — deprecated in CSS Speech Module Level 1 but Lexilla's
/// `LexCSS` still recognises them and Notepad++'s langs.model.xml
/// ships them, so Code++ preserves parity), table layout
/// (`table-layout` / `border-collapse` / `border-spacing` /
/// `caption-side` / `empty-cells`), generated content
/// (`content` / `counter-increment` / `counter-reset` / `quotes` /
/// `marker-offset`), paged media (`page` / `page-break-before` /
/// `page-break-after` / `page-break-inside` / `orphans` / `widows` /
/// `marks` / `size`), per-edge `border-*-color` / `border-*-style`,
/// outline (`outline` + 3 longhands), `cursor`, `min-/max-` width +
/// height, font sizing (`font-size-adjust` / `font-stretch`),
/// bidirectional text (`direction` / `unicode-bidi`), and CSS2's
/// `text-shadow` (relocated to CSS Text Decoration Level 3, but
/// originally CSS2).
///
/// Sourced from the W3C CSS 2.1 Recommendation, cross-checked
/// against Notepad++ baseline.
pub const CSS_PROPERTIES_CSS2: &str = concat!(
    // Aural / speech (CSS2 + CSS Speech)
    "azimuth ",
    // Border per-edge (colour + style — width is class 0)
    "border-bottom-color border-bottom-style border-collapse ",
    "border-left-color border-left-style border-right-color ",
    "border-right-style border-spacing border-top-color border-top-style ",
    // Positioning + clipping + visibility
    "bottom caption-side clip ",
    // Generated content + counters + quotes + marker
    "content counter-increment counter-reset ",
    "cue cue-after cue-before ",
    "cursor ",
    "direction elevation empty-cells ",
    // Font extensions
    "font-size-adjust font-stretch ",
    // Positioning + marker + paged-media marks
    "left marker-offset marks ",
    // Sizing constraints
    "max-height max-width min-height min-width ",
    // Paged media
    "orphans ",
    // Outline
    "outline outline-color outline-style outline-width ",
    // Display extension
    "overflow ",
    // Paged-media controls
    "page page-break-after page-break-before page-break-inside ",
    // Aural pacing + pitch + duration
    "pause pause-after pause-before pitch pitch-range play-during ",
    // Positioning + generated content + paged-media + aural
    "position quotes richness right size ",
    "speak speak-header speak-numeral speak-punctuation speech-rate stress ",
    // Table layout + text shadow + positioning + bidi + visibility +
    // aural voice + paged margins + sizing
    "table-layout text-shadow top unicode-bidi visibility ",
    "voice-family volume widows z-index",
);

/// Space-separated CSS3 + modern property names installed via
/// `LexCSS`'s `SCI_SETKEYWORDS(3, ...)` — class 3 of
/// `cssWordListDesc[]`. Drives `SCE_CSS_IDENTIFIER3` (the third hit
/// in the four-way property-name cascade, Keyword bold).
///
/// **All entries must be lowercase** (same `GetCurrentLowered`
/// contract).
///
/// **Cascade extension of [`CSS_PROPERTIES_CSS1`] + [`CSS_PROPERTIES_CSS2`].**
/// `LexCSS.cxx:433` falls through to class 3 only when classes 0 + 2
/// don't match. No token appears in any other class.
///
/// Covers the CSS3+ modules in widespread use: flexbox (`flex` +
/// `flex-*` + `justify-*` + `align-*` + `order`), grid (`grid` +
/// `grid-*` + `gap` + `*-gap` + `place-*`), transforms (`transform` /
/// `translate` / `rotate` / `scale` + 3D variants), transitions
/// (`transition` + `transition-*`), animations (`animation` +
/// `animation-*`), borders L3 (`border-radius` + corner-radius
/// longhands + image + per-side `*-block-*` / `*-inline-*` logical
/// equivalents), backgrounds L3 (`background-clip` / `-origin` /
/// `-size` / `-blend-mode`), columns / multicol (`columns` +
/// `column-*`), containment (`contain` + `container-*` +
/// `content-visibility`), filter / mask / clip-path, fonts L4
/// (`font-feature-settings` / `font-kerning` /
/// `font-variation-settings` / `font-display`), logical properties
/// (`inset-*` / `margin-block-*` / `margin-inline-*` /
/// `padding-block-*` / `padding-inline-*` / `block-size` /
/// `inline-size` / `max-block-size` etc.), overflow + scroll-snap +
/// scrollbar styling, text L3+ (`text-shadow` is class 2 but
/// `text-decoration-*` longhands + `text-emphasis-*` +
/// `text-underline-*` + `text-justify` + `text-orientation` are
/// here), accessibility (`accent-color` / `caret-color` /
/// `color-scheme`), legacy-but-popular (`opacity` / `line-clamp` /
/// `zoom` / `pointer-events` / `user-select`).
///
/// **Adversarial-verifier additions** beyond the initial synthesis:
/// `opacity` (CSS Color Module Level 3 — single highest-impact
/// omission flagged by both correctness + completeness verifiers),
/// `accent-color` (modern form-control theming), `outline-offset`
/// (CSS3 Basic UI), `scrollbar-color` / `scrollbar-width` /
/// `scrollbar-gutter` (CSS Scrollbars Module, mainstream as of
/// 2024), `content-visibility` (CSS Containment Level 2),
/// `font-display` (CSS Fonts Module Level 4 — ubiquitous in
/// `@font-face` blocks), `line-clamp` (formerly
/// `-webkit-line-clamp`, now standardised).
///
/// Sourced from the W3C CSS Snapshot 2024, W3C CSS Working Group
/// module index, and MDN's CSS property reference. Cross-checked
/// against Notepad++ baseline.
pub const CSS_PROPERTIES_CSS3: &str = concat!(
    // Accessibility + accent
    "accent-color ",
    // Flexbox + grid alignment
    "align-content align-items align-self ",
    // Universal reset
    "all ",
    // Animations
    "animation animation-delay animation-direction animation-duration ",
    "animation-fill-mode animation-iteration-count animation-name ",
    "animation-play-state animation-timing-function ",
    // Form-control native styling
    "appearance ",
    // Layout / containment / sizing
    "aspect-ratio backdrop-filter backface-visibility ",
    // Background L3
    "background-blend-mode background-clip background-origin background-size ",
    // Logical sizing
    "block-size ",
    // Border block (logical)
    "border-block border-block-color border-block-end border-block-end-color ",
    "border-block-end-style border-block-end-width border-block-start ",
    "border-block-start-color border-block-start-style ",
    "border-block-start-width border-block-style border-block-width ",
    // Border corner radii
    "border-bottom-left-radius border-bottom-right-radius ",
    "border-end-end-radius border-end-start-radius ",
    // Border image
    "border-image border-image-outset border-image-repeat border-image-slice ",
    "border-image-source border-image-width ",
    // Border inline (logical)
    "border-inline border-inline-color border-inline-end ",
    "border-inline-end-color border-inline-end-style border-inline-end-width ",
    "border-inline-start border-inline-start-color border-inline-start-style ",
    "border-inline-start-width border-inline-style border-inline-width ",
    // Border radius
    "border-radius border-start-end-radius border-start-start-radius ",
    "border-top-left-radius border-top-right-radius ",
    // Box
    "box-decoration-break box-shadow box-sizing ",
    // Multi-column + break
    "break-after break-before break-inside ",
    // Caret + clip-path
    "caret-color clip-path color-scheme ",
    // Columns / multicol
    "column-count column-fill column-gap column-rule column-rule-color ",
    "column-rule-style column-rule-width column-span column-width columns ",
    // Containment + container queries
    "contain container container-name container-type content-visibility ",
    // Filter + flex
    "filter flex flex-basis flex-direction flex-flow flex-grow flex-shrink ",
    "flex-wrap ",
    // Font features (CSS Fonts Level 3+4)
    "font-display font-feature-settings font-kerning font-variation-settings ",
    // Grid
    "gap grid grid-area grid-auto-columns grid-auto-flow grid-auto-rows ",
    "grid-column grid-column-end grid-column-gap grid-column-start grid-gap ",
    "grid-row grid-row-end grid-row-gap grid-row-start grid-template ",
    "grid-template-areas grid-template-columns grid-template-rows ",
    // Hyphens + image
    "hyphens image-rendering ",
    // Logical sizing + inset
    "inline-size inset inset-block inset-block-end inset-block-start ",
    "inset-inline inset-inline-end inset-inline-start ",
    // Isolation + justification
    "isolation justify-content justify-items justify-self ",
    // Line-clamp (formerly -webkit-line-clamp, now standardised)
    "line-clamp ",
    // Margin logical
    "margin-block margin-block-end margin-block-start margin-inline ",
    "margin-inline-end margin-inline-start ",
    // Mask
    "mask mask-clip mask-composite mask-image mask-mode mask-origin ",
    "mask-position mask-repeat mask-size mask-type ",
    // Max / min logical sizing
    "max-block-size max-inline-size min-block-size min-inline-size ",
    // Misc visual
    "mix-blend-mode ",
    // Object fit
    "object-fit object-position ",
    // Motion path
    "offset offset-anchor offset-distance offset-path offset-position ",
    "offset-rotate ",
    // Opacity (must-fix add per correctness verifier)
    "opacity ",
    // Order + outline-offset
    "order outline-offset ",
    // Overflow
    "overflow-anchor overflow-block overflow-inline overflow-wrap ",
    "overflow-x overflow-y ",
    // Overscroll
    "overscroll-behavior overscroll-behavior-block ",
    "overscroll-behavior-inline overscroll-behavior-x overscroll-behavior-y ",
    // Padding logical
    "padding-block padding-block-end padding-block-start padding-inline ",
    "padding-inline-end padding-inline-start ",
    // Perspective
    "perspective perspective-origin ",
    // Place (flexbox+grid shorthand)
    "place-content place-items place-self ",
    // Pointer / resize / rotate
    "pointer-events resize rotate row-gap scale ",
    // Scroll
    "scroll-behavior ",
    "scroll-margin scroll-margin-block scroll-margin-block-end ",
    "scroll-margin-block-start scroll-margin-bottom scroll-margin-inline ",
    "scroll-margin-inline-end scroll-margin-inline-start scroll-margin-left ",
    "scroll-margin-right scroll-margin-top ",
    "scroll-padding scroll-padding-block scroll-padding-block-end ",
    "scroll-padding-block-start scroll-padding-bottom scroll-padding-inline ",
    "scroll-padding-inline-end scroll-padding-inline-start scroll-padding-left ",
    "scroll-padding-right scroll-padding-top ",
    "scroll-snap-align scroll-snap-stop scroll-snap-type ",
    // Scrollbar styling (CSS Scrollbars Module)
    "scrollbar-color scrollbar-gutter scrollbar-width ",
    // Tab + text decoration / emphasis
    "tab-size text-align-last ",
    "text-decoration-color text-decoration-line text-decoration-skip-ink ",
    "text-decoration-style text-decoration-thickness ",
    "text-emphasis text-emphasis-color text-emphasis-position text-emphasis-style ",
    "text-justify text-orientation text-overflow text-rendering ",
    "text-underline-offset text-underline-position ",
    // Touch + transforms
    "touch-action transform transform-box transform-origin transform-style ",
    // Transitions
    "transition transition-delay transition-duration transition-property ",
    "transition-timing-function translate ",
    // User select + will-change
    "user-select will-change ",
    // Word + writing + zoom
    "word-break word-wrap writing-mode zoom",
);

/// Space-separated CSS pseudo-element names installed via `LexCSS`'s
/// `SCI_SETKEYWORDS(4, ...)` — class 4 of `cssWordListDesc[]`. Drives
/// `SCE_CSS_PSEUDOELEMENT` (mapped to Keyword2). Stored WITHOUT
/// leading colons — the lexer's `:` state-machine matches the bare
/// identifier after either single-colon `:before` (legacy CSS2) or
/// double-colon `::before` (CSS3+) prefix.
///
/// **All entries must be lowercase** (same `GetCurrentLowered`
/// contract).
///
/// **Legitimate state-disambiguated cross-namespace overlap.** `cue`
/// appears here (pseudo-element `::cue` for `WebVTT`) AND in
/// [`CSS_PROPERTIES_CSS2`] (the aural property `cue: ...`). Lexilla
/// disambiguates by lexer state — class 4 lookup only fires
/// post-`:` (or `::`) in the SELECTOR position, class 2 lookup only
/// fires in the PROPERTY-NAME position — so the same token in both
/// lists is the correct representation, not a duplicate to remove.
///
/// Covers W3C CSS Pseudo-Elements Level 4: typographic
/// (`before` / `after` / `first-line` / `first-letter` / `marker` /
/// `selection`), form controls (`placeholder` /
/// `file-selector-button`), media (`backdrop` / `cue` /
/// `cue-region` / `slotted` / `part`), web platform
/// (`view-transition` family — 4 entries for the View Transitions
/// API), accessibility / editor (`spelling-error` /
/// `grammar-error` / `target-text`).
///
/// Sourced from W3C CSS Pseudo-Elements Module Level 4 + CSS View
/// Transitions Module Level 1, cross-checked against MDN's
/// pseudo-element reference.
pub const CSS_PSEUDO_ELEMENTS: &str = concat!(
    "after backdrop before ",
    "cue cue-region ",
    "file-selector-button ",
    "first-letter first-line ",
    "grammar-error marker ",
    "part placeholder selection slotted spelling-error ",
    "target-text ",
    "view-transition view-transition-group view-transition-image-pair ",
    "view-transition-new view-transition-old",
);

/// Space-separated Perl reserved-word + built-in vocabulary installed
/// via `LexPerl`'s `SCI_SETKEYWORDS(0, ...)` — class 0 of the
/// single-slot `perlWordListDesc[]`. Drives `SCE_PL_WORD` (mapped to
/// Keyword bold blue).
///
/// **CRITICAL: mixed-case wordlist with strict load-bearing UPPERCASE
/// entries.** `LexPerl.cxx:96-104` (`isPerlKeyword`) copies token
/// bytes verbatim into a stack buffer and calls `keywords.InList(s)`
/// with **no case folding**. Perl source spells the phase-block names
/// (`BEGIN` / `END` / `INIT` / `CHECK` / `UNITCHECK` / `AUTOLOAD` /
/// `DESTROY`) and the `__TOKEN__` family (`__FILE__` / `__LINE__` /
/// `__PACKAGE__` / `__SUB__` / `__DATA__` / `__END__`) in uppercase
/// by language requirement — there is no lowercase form in any real
/// Perl source. The wordlist MUST store the uppercase form for
/// these 13 tokens. Lowercase forms would silently disable the
/// highlight. All other entries are lowercase per standard Perl
/// convention.
///
/// **`__DATA__` / `__END__` are load-bearing for `SCE_PL_DATASECTION`
/// styling.** `LexPerl.cxx:872-877` only recolours these markers
/// (and everything after them) to `SCE_PL_DATASECTION` from inside
/// the `SCE_PL_WORD` state, which is only entered after a successful
/// wordlist hit. Without uppercase `__DATA__` / `__END__` in this
/// wordlist, the trailing data section never picks up the
/// de-emphasised paint — it renders as plain identifier text.
///
/// **Single wordlist class.** `perlWordListDesc[]` declares one
/// `"Keywords"` slot. The list bundles the standard Perl vocabulary:
/// control-flow keywords + declarators (`if`, `unless`, `while`,
/// `for`, `foreach`, `do`, `return`, `goto`, `die`, `exit`, `my`,
/// `our`, `local`, `state`, `package`, `use`, `require`, `no`,
/// `sub`, `bless`, `ref`, `defined`, `undef`, `wantarray`), the
/// phase-block names UPPERCASE, the `__TOKEN__` family UPPERCASE,
/// named operators (`x` for repetition, `cmp`, `lt`, `gt`, `le`,
/// `ge`, `eq`, `ne`, `and`, `or`, `not`, `xor`, `err`), modern
/// post-5.10 vocabulary (`say`, `state`, `given`, `when`, `default`,
/// `break`, `fc`, `isa`), and the quote-like operator names (`m`,
/// `s`, `y`, `q`, `qq`, `qx`, `qr`, `qw`, `tr`) which trigger
/// state-machine transitions but are themselves keywords.
///
/// Coverage continues with the full I/O family (`print`, `printf`,
/// `sprintf`, `open`, `close`, `read`, `write`, `seek`, `tell`,
/// `binmode`, `fileno`, `truncate`, `eof`, `getc`, `chomp`, `chop`,
/// `chr`, `ord`, `lc`, `lcfirst`, `uc`, `ucfirst`, `hex`, `oct`),
/// string + regex built-ins (`length`, `substr`, `index`, `rindex`,
/// `pos`, `split`, `join`, `reverse`, `pack`, `unpack`, `quotemeta`,
/// `study`), list / array / hash built-ins (`push`, `pop`, `shift`,
/// `unshift`, `splice`, `sort`, `grep`, `map`, `keys`, `values`,
/// `each`, `exists`, `delete`), math built-ins (`abs`, `int`,
/// `rand`, `srand`, `sqrt`, `sin`, `cos`, `exp`, `log`, `atan2`),
/// the full syscall + IPC + process family, and the POSIX
/// pwent/grent/netent/protoent/servent traversal verbs. Finally the
/// Carp prose-diagnostics (`carp`, `croak`, `confess`, `cluck`) —
/// these are module imports rather than core built-ins, so a
/// user-defined `sub carp { ... }` will render bold-blue when it
/// would otherwise render as default. Accepted false-positive risk;
/// `LexPerl` has only one wordlist class so there is no Keyword2
/// promotion path.
///
/// **Deliberate exclusions:** sigils (`$` / `@` / `%` / `&` / `*`)
/// — those are operator-character tokens, not wordlist entries; the
/// lexer routes them to SCALAR / ARRAY / HASH / SYMBOLTABLE styles
/// based on the trailing identifier. File-test operators
/// (`-e` / `-f` / `-d` / `-r` / `-w` / `-x` / `-s` / `-T` / etc.)
/// — these are operator+letter pairs tokenised by lexer state, not
/// keyword lookups. Special variables (`$_` / `@ARGV` / `%ENV` /
/// `$0` / `$!` / `$@` / etc.) — these route to SCALAR / ARRAY / HASH
/// styles. Package-qualified names (`File::Spec::catfile`) — module
/// imports, not wordlist territory.
///
/// Sourced from `perlfunc(1)` + `perlsyn(1)` + Notepad++'s shipped
/// `langs.model.xml` `<Language name="perl">` `instre1` list, and
/// adversarially verified across three lenses (Perl docs, N++
/// conventions, Lexilla source). Adversarial-verifier MUST-FIX
/// additions before commit: 7 UPPERCASE phase blocks + 6 UPPERCASE
/// `__TOKEN__` family + missing `ge` operator (13 + 1 = 14 additions
/// from the initial synthesis-round 245 → 259).
pub const PERL_KEYWORDS: &str = concat!(
    // Math built-ins (alphabetical block starter)
    "abs ",
    // IPC + system calls (accept...alarm)
    "accept alarm ",
    // Boolean low-precedence operator + math
    "and atan2 ",
    // IPC
    "bind binmode ",
    // OO / declaration
    "bless break ",
    // Diagnostics / introspection
    "caller ",
    // System
    "chdir chmod chomp chop chown chr chroot ",
    "close closedir ",
    // String comparison + IPC
    "cmp connect continue cos crypt ",
    // DBM
    "dbmclose dbmopen ",
    // Modern Perl switch + introspection
    "default defined delete die do dump ",
    // List + hash iteration
    "each ",
    // Conditional + ent-traversal
    "else elsif ",
    "endgrent endhostent endnetent endprotoent endpwent endservent ",
    "eof eq err eval exec exists exit exp ",
    // Modern Perl 5.16+ foldcase + system
    "fc fcntl fileno flock ",
    // Loops + format
    "for foreach fork format formline ",
    // I/O + ent-traversal
    "getc ",
    "getgrent getgrgid getgrnam ",
    "gethostbyaddr gethostbyname gethostent getlogin ",
    "getnetbyaddr getnetbyname getnetent ",
    "getpeername getpgrp getppid getpriority ",
    "getprotobyname getprotobynumber getprotoent ",
    "getpwent getpwnam getpwuid ",
    "getservbyname getservbyport getservent ",
    "getsockname getsockopt ",
    // Modern switch + glob + time + jump
    "ge ",
    "given glob gmtime goto grep gt ",
    // Math + control
    "hex ",
    "if index int ioctl isa ",
    "join ",
    "keys kill ",
    // Loop control + string
    "last lc lcfirst le length link listen local localtime lock log lstat lt ",
    // Quote-like operator names + iteration
    "m map mkdir ",
    // IPC msg + declaration
    "msgctl msgget msgrcv msgsnd ",
    "my ",
    "ne next no not ",
    // Numeric conversion
    "oct open opendir or ord our ",
    // Pack + IPC
    "pack package pipe pop pos print printf prototype push ",
    // Quote-like operator names
    "q qq qr quotemeta qw qx ",
    // Math + I/O + introspection
    "rand read readdir readline readlink readpipe ",
    "recv redo ref rename require reset return reverse rewinddir rindex rmdir ",
    // Quote-like operator (substitution) + modern Perl
    "s say scalar seek seekdir select ",
    // IPC sem + setN-ent
    "semctl semget semop send ",
    "setgrent sethostent setnetent setpgrp setpriority setprotoent setpwent ",
    "setservent setsockopt ",
    // Array + IPC shm + syscall
    "shift ",
    "shmctl shmget shmread shmwrite ",
    "shutdown sin sleep socket socketpair sort splice split sprintf sqrt srand ",
    "stat state study sub substr symlink syscall sysopen sysread sysseek system ",
    "syswrite ",
    // Tied I/O + time + traversal
    "tell telldir tie tied time times tr truncate ",
    "uc ucfirst umask undef unless unlink unpack unshift untie until use utime ",
    // Misc system + values
    "values vec ",
    "wait waitpid wantarray warn when while write ",
    // Repetition operator + low-precedence boolean
    "x xor ",
    // Quote-like operator name (legacy synonym for tr)
    "y ",
    // Carp prose-diagnostics (idiomatic; false-positive accepted)
    "carp croak confess cluck ",
    // Phase-block special subroutines — UPPERCASE per Perl spec
    // (lexer is byte-exact; lowercase would never match)
    "BEGIN END INIT CHECK UNITCHECK AUTOLOAD DESTROY ",
    // __TOKEN__ family — UPPERCASE per Perl spec, load-bearing for
    // SCE_PL_DATASECTION mapping (__DATA__ / __END__ MUST be matched
    // to enter the DATASECTION state per LexPerl.cxx:872-877)
    "__FILE__ __LINE__ __PACKAGE__ __SUB__ __DATA__ __END__",
);

/// Space-separated Lua 5.4 reserved-word vocabulary installed via
/// `LexLua`'s `SCI_SETKEYWORDS(0, ...)` — class 0 of
/// `luaWordListDesc[]`. Drives `SCE_LUA_WORD` (mapped to Keyword
/// bold blue).
///
/// **Case-sensitive lexer.** `LexLua.cxx:472, 479` calls
/// `keywords.InList(identifier)` with no case folding — verified by
/// zero matches for `tolower` / `MakeLowerCase` / `GetCurrentLowered`
/// in the source AND by inspection of `WordList::InList` at
/// `vendor/lexilla/lexlib/WordList.cxx:162-170, 202-204` which does
/// byte-exact comparison. Identifier text is captured raw via
/// `sc.GetCurrentString(s, Transform::none)` at `LexLua.cxx:391`.
/// Wordlists must store source-canonical casing — Lua language
/// semantics: every reserved keyword is lowercase (`if`, `then`,
/// `end`, `function`, `local`, `goto`, `return`, …). Same byte-exact
/// contract as [`PERL_KEYWORDS`] / [`PYTHON_KEYWORDS`].
///
/// **Two-class wordlist with [`LUA_KEYWORDS_2`] for m1.** Class 0
/// holds the 22 Lua 5.4 reserved words (exactly the §3.1 set from
/// the Lua reference manual). Class 1 holds 25 basic library
/// function names from the `_G` table (Lua 5.4 §6.1). Lexilla
/// checks class 0 first (`LexLua.cxx:472,479-480`), so a collision
/// would silently demote class 1 entries — pinned no-overlap
/// structurally in the Lua theme test.
///
/// **8-class wordlist surface, only 2 populated for m1.**
/// `luaWordListDesc[]` at `LexLua.cxx:51-61` declares 8 wordlist
/// slots — class 0 "Keywords", class 1 "Basic functions", class 2
/// "String, (table) & math functions", class 3 "(coroutines), I/O
/// & system facilities", classes 4-7 "user1..user4". The dispatch
/// chain at `LexLua.cxx:479-494` consumes them in that exact
/// order; an out-of-order install (e.g. installing basic-function
/// names into class 0) silently mis-classifies them as Keyword
/// instead of Keyword2. Code++ m1 installs classes 0 + 1 only,
/// matching the Python wiring precedent. The string + table +
/// math library member names (target: class 2) and coroutine +
/// io + os + debug member names (target: class 3) are tracked as
/// a follow-on commit — they add `LUA_KEYWORDS_3` /
/// `LUA_KEYWORDS_4` constants and route to `SCE_LUA_WORD3` /
/// `WORD4` (already pre-themed to Keyword2 in `LUA_STYLES`, so
/// wiring is a single line in `LUA_THEME`). The four user
/// customisation slots (classes 4 through 7) stay empty by design.
///
/// **`goto` placement, load-bearing for `SCE_LUA_LABEL`.** Class
/// 0 includes `goto` — the label-from-goto-target lexer path at
/// `LexLua.cxx:382-396` requires `goto` to be in `keywords`
/// (class 0). If `goto` is missing from class 0, the
/// `goto target_name` construct silently never highlights
/// `target_name` as `SCE_LUA_LABEL`. The `::label::` definition
/// path at `LexLua.cxx:320-357` ALSO consults class 0 via the
/// `!keywords.InList(s)` guard at `:335` — rejecting any
/// `::reserved_word::` as not-a-label. Both behaviours are
/// correct and require `goto` to live in class 0.
///
/// **`true` / `false` / `nil` placement: class 0.** Lua's three
/// special literals are spelled lowercase and are language-level
/// reserved words (you cannot write `local true = 1`). Same byte-
/// exact lowercase as the rest of class 0 — no Python-2-style
/// builtin / reserved-word ambiguity here.
///
/// Sourced from the Lua 5.4 Reference Manual §3.1 ("Lexical
/// Conventions / Reserved Words"). Cross-referenced against
/// `vendor/lexilla/lexers/LexLua.cxx:51-61` for the wordlist
/// class taxonomy and N++'s shipped `langs.model.xml` `<Language
/// name="lua">` `instre1` list for default-set parity.
pub const LUA_KEYWORDS: &str = concat!(
    // Logical + boolean literals (lowercase per Lua spec — the
    // lexer is byte-exact, capitalised forms never match)
    "and false nil not or true ",
    // Block / loop / control flow
    "break do else elseif end for if in repeat return then until while ",
    // Declaration + jump
    "function goto local",
);

/// Space-separated Lua 5.4 basic library function vocabulary
/// installed via `LexLua`'s `SCI_SETKEYWORDS(1, ...)` — class 1
/// of `luaWordListDesc[]` ("Basic functions"). Drives
/// `SCE_LUA_WORD2` (mapped to Keyword2 steel-blue).
///
/// **Case preserved per Lua source convention.** All entries are
/// lowercase except the two module-introspection sentinels
/// `_G` and `_VERSION`, which Lua canonically writes with a
/// leading underscore + uppercase. The lexer is byte-exact —
/// `print` matches but `Print` does not, `_G` matches but `_g`
/// does not.
///
/// **Scope: basic library only (the `_G` table).** Covers the 25
/// global functions and sentinels from the Lua 5.4 standard
/// library §6.1 ("Basic Functions"). String / table / math
/// library member names (`string.format`, `table.insert`,
/// `math.floor`, …) are DEFERRED to a future `LUA_KEYWORDS_3`
/// targeting wordlist class 2 — see `LUA_KEYWORDS` doc comment
/// for the rationale. Coroutine / I/O / OS / debug library
/// names similarly deferred to `LUA_KEYWORDS_4` targeting
/// class 3. Both pre-themed to Keyword2 in `LUA_STYLES` for
/// zero-effort activation.
///
/// **`type` placement: class 1 ONLY.** Both `type(v)` (basic
/// function) and `math.type` / `io.type` (library member names)
/// exist in Lua 5.4. With Code++ shipping classes 0 + 1 today,
/// `type` lives only in class 1 — the future `LUA_KEYWORDS_3`
/// must NOT re-add it (Lexilla checks class 0 first, then 1,
/// then 2 in source order; a cross-class duplicate would silently
/// demote the secondary entry).
///
/// **`getmetatable` / `setmetatable` placement: class 1 ONLY.**
/// `debug.getmetatable` / `debug.setmetatable` exist in the
/// `debug` library but the bare names belong to the basic
/// library — class 1 carries the bare name; the future
/// `LUA_KEYWORDS_4` covering `debug.*` must NOT re-add them.
///
/// **No cross-class duplicates with [`LUA_KEYWORDS`].** Verified
/// by `HashSet` intersection before commit AND structurally
/// pinned by the Lua theme test. Lexilla checks class 0 first
/// (`LexLua.cxx:472, 479-480`), so a duplicate would silently
/// demote class 1 entries to Keyword instead of Keyword2 — an
/// invisible bug.
///
/// Sourced from the Lua 5.4 Reference Manual §6.1 ("Basic
/// Functions"), cross-referenced against `dofile -e
/// 'for k in pairs(_G) do print(k) end'` against a stock
/// Lua 5.4 interpreter, and N++'s shipped `langs.model.xml`
/// `<Language name="lua">` `instre2` / type1 list for default-
/// set parity. The N++ file is referenced for parity inspection
/// only — no content copied from it (per the CLAUDE.md
/// "no code from Notepad++" rule); the canonical source for
/// every entry below is the Lua 5.4 Reference Manual.
pub const LUA_KEYWORDS_2: &str = concat!(
    // Module / version sentinels (canonical _G / _VERSION casing)
    "_G _VERSION ",
    // Type / metaprotocol introspection
    "assert collectgarbage error getmetatable rawequal rawget rawlen ",
    "rawset setmetatable tonumber tostring type ",
    // Iteration helpers
    "ipairs next pairs select ",
    // I/O + module loading
    "dofile load loadfile print require ",
    // Error handling + sub-call
    "pcall xpcall",
);

/// Space-separated Common Lisp / Scheme function + special-operator
/// vocabulary installed via `LexLisp`'s `SCI_SETKEYWORDS(0, …)` — class
/// 0 of `lispWordListDesc[]` at `vendor/lexilla/lexers/LexLisp.cxx:280-284`
/// ("Functions and special operators"). Drives `SCE_LISP_KEYWORD` per
/// the dispatch at `LexLisp.cxx:64-65`.
///
/// **Two-class wordlist with [`LISP_KEYWORDS_KW`] for class 1.** Class
/// 0 first-match-wins over class 1 per `LexLisp.cxx:64-68` (Lexilla
/// checks `keywords.InList(s)` before `keywords_kw.InList(s)`) — a
/// token duplicated across classes silently demotes the class-1 entry.
/// Cross-class no-overlap pinned in `lisp_uses_lexlisp_two_class_theme`.
///
/// **Case-sensitive lexer.** `classifyWordLisp` at `LexLisp.cxx:50-75`
/// builds the token buffer via raw `styler[start + i]` at `:56` with no
/// case folding — `defun` matches, `DEFUN` never does. Common Lisp
/// source convention is lowercase; wordlists must match. Same
/// byte-exact contract as [`LUA_KEYWORDS`] / [`TCL_KEYWORDS`].
///
/// **`:`-prefix stripping does NOT happen here.** In Lisp, `:foo`
/// enters `SCE_LISP_SYMBOL` (state 5) via the DEFAULT-state branch at
/// `LexLisp.cxx:107-109` — it never reaches `classifyWordLisp`. So
/// keyword-argument names (`:test`, `:key`, `:initial-value`) DO NOT
/// belong in this wordlist; they paint via SYMBOL / Lifetime
/// automatically.
///
/// Sourced from the Common Lisp `HyperSpec` ("Symbols in the
/// `COMMON-LISP` Package"). Cross-referenced against Notepad++'s
/// `langs.model.xml` `<Language name="lisp">` `instre1` list for
/// default-set parity (no content copied from Notepad++ per
/// CLAUDE.md).
pub const LISP_KEYWORDS: &str = concat!(
    "not defun defmacro defvar defparameter defconstant defclass ",
    "defmethod defgeneric defsetf defstruct deftype defpackage ",
    "define-condition define-symbol-macro define-modify-macro ",
    "define-compiler-macro define-setf-expander ",
    "+ - * / = < > <= >= /= 1+ 1- ",
    "princ prin1 print pprint write format terpri fresh-line ",
    "eval apply funcall quote identity function complement backquote ",
    "lambda set setq setf psetq psetf multiple-value-setq ",
    "gensym make-symbol intern symbol-name symbol-value symbol-plist ",
    "get getf putprop remprop hash make-hash-table gethash remhash ",
    "array make-array aref svref elt ",
    "car cdr cons list list* append reverse nreverse last nth nthcdr ",
    "first second third fourth fifth sixth seventh eighth ninth tenth ",
    "caar cadr cdar cddr caaar caadr cadar caddr cdaar cdadr cddar ",
    "cdddr caaaar caaadr caadar caaddr cadaar cadadr caddar cadddr ",
    "cdaaar cdaadr cdadar cdaddr cddaar cddadr cdddar cddddr ",
    "member assoc rassoc subst sublis nsubst nsublis remove remove-if ",
    "remove-if-not delete delete-if delete-if-not length position ",
    "position-if find find-if count count-if ",
    "mapc mapcar mapl maplist mapcan mapcon reduce ",
    "rplaca rplacd nconc revappend nreconc ",
    "atom symbolp numberp integerp floatp rationalp complexp realp ",
    "stringp characterp arrayp vectorp listp consp null boundp ",
    "fboundp functionp keywordp packagep hash-table-p typep subtypep ",
    "minusp zerop plusp evenp oddp eq eql equal equalp ",
    "cond case ecase typecase etypecase when unless ",
    "and or if let let* flet labels macrolet symbol-macrolet ",
    "prog prog1 prog2 progn progv block return return-from tagbody go ",
    "do do* dolist dotimes loop with for in on across being finally ",
    "catch throw unwind-protect handler-case handler-bind ",
    "restart-case restart-bind signal error cerror warn break ",
    "continue errset backtrace evalhook ",
    "truncate floor ceiling round mod rem float coerce ",
    "min max abs signum sin cos tan asin acos atan sinh cosh tanh ",
    "expt exp log sqrt isqrt random ",
    "logand logior logxor lognot logeqv lognand lognor logorc1 ",
    "logorc2 logandc1 logandc2 logtest logbitp logcount ash ",
    "integer bignum ratio rational real complex character ",
    "declare declaim proclaim the check-type assert ",
    "eval-when in-package use-package import export ",
    "shadow shadowing-import ",
    "multiple-value-bind multiple-value-call multiple-value-list ",
    "values values-list ",
    "read read-line read-char write-char write-line write-string ",
    "open close with-open-file with-open-stream ",
    "make-instance slot-value slot-boundp with-slots with-accessors ",
    "call-next-method next-method-p ",
    "t nil",
);

/// Space-separated Common Lisp lambda-list-marker vocabulary installed
/// via `LexLisp`'s `SCI_SETKEYWORDS(1, …)` — class 1 of
/// `lispWordListDesc[]` at `LexLisp.cxx:280-284` ("Keywords"). Drives
/// `SCE_LISP_KEYWORD_KW` per `LexLisp.cxx:66-67`.
///
/// **Scope: `&`-prefixed lambda-list markers ONLY.** The eight ANSI CL
/// markers per CLHS §3.4.1 ("Ordinary Lambda Lists"): `&allow-other-keys`,
/// `&aux`, `&body`, `&environment`, `&key`, `&optional`, `&rest`,
/// `&whole`. The `&` character is admitted by `isLispwordstart` at
/// `LexLisp.cxx:44-47` (excludes only `;`, whitespace, operator chars,
/// newline, `"`), so `&rest` enters `SCE_LISP_IDENTIFIER` at `:110-112`
/// and reaches `classifyWordLisp` with the `&` prefix INCLUDED in the
/// buffer. Wordlist entries MUST retain the leading `&` — parallels
/// [`NSIS_VARIABLES`] storing entries with leading `$`.
///
/// **No cross-class duplicates with [`LISP_KEYWORDS`].** Verified by a
/// `HashSet` intersection guard in `lisp_uses_lexlisp_two_class_theme`.
/// Lexilla's first-match-wins chain at `LexLisp.cxx:64-68` would
/// silently demote any duplicate class-1 entry.
///
/// Notepad++ ships class 1 empty (`<Keywords name="instre2"></Keywords>`
/// in stock `langs.model.xml`). This is a strictly-additive enhancement
/// relative to Notepad++'s colouring — nothing that Notepad++ paints
/// changes; tokens Notepad++ leaves as IDENTIFIER get promoted to
/// `KEYWORD_KW`.
pub const LISP_KEYWORDS_KW: &str =
    "&allow-other-keys &aux &body &environment &key &optional &rest &whole";

// --- Assembly (LexAsm) wordlists --------------------------------------
//
// LexAsm at `vendor/lexilla/lexers/LexAsm.cxx` powers L_ASM (`SCLEX_ASM`)
// via the "asm" lexer name in Notepad++'s catalog. The classifier at
// `:329-358` calls `GetCurrentLowered(s, sizeof(s))` at `:332` before
// every `InList` check, so every wordlist below is **lowercase-only**
// by contract — mixed-case source tokens ("MOV", "Mov", "mov") all
// hit the same lowercase entry. This is the ergonomic authoring
// contract asm has always had (assemblers themselves are case-
// insensitive on mnemonics / registers / directives) and matches
// Notepad++'s stock `langs.model.xml <Language name="asm">` list.
//
// The wordlists cover **x86-family (16 / 32 / 64-bit)** assembly as
// the primary target — the dominant use case for a general
// developer editor. NASM's official instruction reference, Intel's
// SDM Volume 2A/2B/2C, and AMD64 APM Volume 3 were cross-referenced
// as source of truth; entries with an asterisk-comment in the
// original references (pseudo-ops, macro directives) are placed in
// the appropriate DIRECTIVES/OPS/EXT class rather than CPU.
//
// **Six populated classes + two empty fold-only ones.** The
// eight-class `asmWordListDesc[]` at `LexAsm.cxx:80-90` is filled
// as follows:
//   * class 0 CPU        → [`ASM_CPU_KEYWORDS`]     (~300 mnemonics)
//   * class 1 FPU        → [`ASM_FPU_KEYWORDS`]     (~95 x87 mnemonics)
//   * class 2 Registers  → [`ASM_REG_KEYWORDS`]     (~240 registers)
//   * class 3 Directives → [`ASM_DIRECTIVE_KEYWORDS`] (~260 MASM +
//                          NASM + GAS + preprocessor directives)
//   * class 4 Operands   → [`ASM_DIRECTIVE_OP_KEYWORDS`] (~35 size /
//                          scope / attribute qualifiers)
//   * class 5 Extended   → [`ASM_EXT_KEYWORDS`]     (~495 SSE / AVX /
//                          AVX-512 / MMX / 3DNow!)
// Classes 6/7 are `Directives4Foldstart` / `Directives4Foldend`,
// consulted only by the folder (`LexAsm.cxx:490-500`); left empty
// today. A future commit can populate them with matched pairs
// (`.if`/`.endif`, `%macro`/`%endmacro`, `proc`/`endp`) to enable
// directive-pair folding without disturbing the classifier.
//
// **No cross-class duplicates.** The first-match-wins chain at
// `:335-347` demotes any duplicate silently — a mnemonic listed in
// both CPU and EXT would paint from whichever class the chain sees
// first (CPU). Verified pairwise by a `HashSet` intersection guard
// in `asm_uses_lexasm_six_class_theme` (see the seen-set assertion
// walking `asm.keywords`).

/// Space-separated **x86-family CPU-instruction** vocabulary
/// installed via `LexAsm`'s `SCI_SETKEYWORDS(0, …)` — class 0 of
/// `asmWordListDesc[]` at `vendor/lexilla/lexers/LexAsm.cxx:80-90`
/// ("CPU instructions"). Drives `SCE_ASM_CPUINSTRUCTION`.
///
/// **Scope: general-purpose, control-flow, and integer arithmetic
/// mnemonics** across 16 / 32 / 64-bit x86. Data movement (`mov`,
/// `push`, `pop`, `lea`, `xchg`), arithmetic (`add`, `sub`, `mul`,
/// `imul`, `div`, `idiv`, `inc`, `dec`, `neg`), logic (`and`, `or`,
/// `xor`, `not`, `shl`, `shr`, `sal`, `sar`, `rol`, `ror`, `test`),
/// compare/branch (`cmp`, `jmp`, `je`, `jne`, `jg`, `jl`, `ja`,
/// `jb`, `jc`, `jz`, `js`, `jo`, `call`, `ret`, `loop`), string
/// ops (`movs`, `stos`, `lods`, `scas`, `cmps`, `rep`, `repe`,
/// `repne`), stack (`push`, `pop`, `pushf`, `popf`, `enter`,
/// `leave`), set-on-condition (`setz`, `setnz`, `sete`, `setne`,
/// `setg`, `setl`, `seta`, `setb`), system (`syscall`, `sysret`,
/// `int`, `iret`, `cpuid`, `rdtsc`, `hlt`, `cli`, `sti`), and
/// misc (`nop`, `wait`, `cbw`, `cwd`, `cdq`, `cqo`, `cld`, `std`,
/// `bswap`, `xlat`).
///
/// **Sourced from Intel SDM Volume 2A/2B/2C** (instruction set
/// reference) and AMD64 APM Volume 3. Cross-referenced against
/// Notepad++'s `langs.model.xml <Language name="asm">` `instre1`
/// list for default-set parity (no content copied from Notepad++
/// per CLAUDE.md — this is an independent enumeration from the
/// public ISA references).
pub const ASM_CPU_KEYWORDS: &str = concat!(
    // Data movement
    "mov movabs movsx movsxd movzx xchg xadd cmpxchg cmpxchg8b cmpxchg16b ",
    "push pusha pushad pushf pushfd pushfq ",
    "pop popa popad popf popfd popfq ",
    "lea lahf sahf lds les lfs lgs lss xlat xlatb bswap ",
    "cmove cmovne cmovnz cmovz cmovg cmovnle cmovge cmovnl cmovl cmovnge ",
    "cmovle cmovng cmova cmovnbe cmovae cmovnb cmovb cmovnae cmovbe cmovna ",
    "cmovc cmovnc cmovo cmovno cmovs cmovns cmovp cmovpe cmovnp cmovpo ",
    // Integer arithmetic
    "add adc sub sbb inc dec neg mul imul div idiv ",
    "adcx adox mulx ",
    // Logic + shift + rotate
    "and or xor not shl shld shr shrd sal sar rol ror rcl rcr test ",
    // Bit ops
    "bt bts btr btc bsf bsr popcnt lzcnt tzcnt andn bextr blsi blsmsk blsr ",
    "bzhi pdep pext rorx sarx shlx shrx ",
    // Compare + branch
    "cmp jmp ",
    "je jne jz jnz jg jng jge jnge jl jnl jle jnle ja jna jae jnae jb jnb jbe jnbe ",
    "jc jnc jo jno js jns jp jpe jnp jpo jcxz jecxz jrcxz ",
    // Loop family
    "loop loope loopne loopz loopnz ",
    // Call / return
    "call ret retn retf iret iretd iretq enter leave ",
    // Set on condition
    "sete setne setz setnz setg setng setge setnge setl setnl setle setnle ",
    "seta setna setae setnae setb setnb setbe setnbe setc setnc seto setno ",
    "sets setns setp setpe setnp setpo ",
    // String
    "movs movsb movsw movsd movsq ",
    "stos stosb stosw stosd stosq ",
    "lods lodsb lodsw lodsd lodsq ",
    "scas scasb scasw scasd scasq ",
    "cmps cmpsb cmpsw cmpsd cmpsq ",
    "rep repe repne repz repnz ",
    // I/O
    "in out ins insb insw insd outs outsb outsw outsd ",
    // Flags / conversion
    "clc cld cli cmc stc std sti ",
    "cbw cwd cwde cdq cdqe cqo ",
    // Segment / descriptor / system
    "arpl bound lar lsl verr verw sgdt sidt sldt smsw str lgdt lidt lldt ltr ",
    "clts invd wbinvd invlpg lmsw hlt rsm ud2 ",
    // CPUID / TSC / MSR / random / rd/wr
    "cpuid rdtsc rdtscp rdmsr wrmsr rdpmc rdrand rdseed rdpid rdgsbase rdfsbase wrgsbase wrfsbase ",
    // Interrupts / syscall
    "int int3 into syscall sysret sysenter sysexit swapgs ",
    // Prefetch hints (SSE / 3DNow! origin). `prefetch` +
    // `prefetchw` are omitted here — routed via ASM_EXT_KEYWORDS
    // to keep them grouped with the rest of the SIMD prefetch
    // family (`prefetchnta`, `prefetcht0..2`). `prefetchwt1` is
    // Intel Xeon Phi / Knights Landing — also EXT.
    // Misc no-op / hint. `pause` stays as CPU (spin-loop hint —
    // classified in Notepad++'s default list under CPU too, even
    // though the encoding is SSE2's REP NOP). `fwait` is x87 FPU
    // sync — lives in ASM_FPU_KEYWORDS to avoid a class-0/class-1
    // duplicate the LexAsm classifier chain at :335-347 would
    // silently demote. `wait` (opcode 9B) stays here as the CPU
    // sync instruction.
    "nop pause ud0 ud1 endbr32 endbr64 wait ",
    // 64-bit cache-management + non-temporal store. `movnti` (SSE2-
    // introduced but scalar-integer non-temporal store) counts as
    // CPU domain per Intel SDM Vol. 2 — kept here rather than in
    // EXT. `clflush` / `clflushopt` / `clwb` are cache-management
    // and reasonably CPU. `popcnt` is already listed in the bit-ops
    // group above (SSE4.2 introduction but scalar bit operation);
    // that placement wins the first-match here.
    "movnti clflush clflushopt clwb ",
);

/// Space-separated **x87 FPU mnemonic** vocabulary installed via
/// `LexAsm`'s `SCI_SETKEYWORDS(1, …)` — class 1 of
/// `asmWordListDesc[]` at `vendor/lexilla/lexers/LexAsm.cxx:80-90`
/// ("FPU instructions"). Drives `SCE_ASM_MATHINSTRUCTION`.
///
/// **Scope: x87 floating-point unit ONLY** — the classic ST(0)/ST(7)
/// stack-based ISA (Intel SDM Volume 2 Ch. 3-6, x87 sections).
/// SSE / SSE2 / AVX floating-point instructions live in
/// [`ASM_EXT_KEYWORDS`] (class 5) as "extended instructions" — that
/// classification matches Notepad++'s stock list and reflects the
/// visual grouping most assembly programmers reach for. MMX
/// integer-vector mnemonics ALSO live in EXT (they share SSE's
/// register file conceptually and the classifier chains through EXT
/// after this class).
pub const ASM_FPU_KEYWORDS: &str = concat!(
    // Load / store
    "fld fst fstp fild fist fistp fisttp fbld fbstp ",
    "fldz fld1 fldpi fldl2e fldl2t fldlg2 fldln2 ",
    "fxch fcmove fcmovne fcmovb fcmovbe fcmovnb fcmovnbe fcmovu fcmovnu ",
    // Arithmetic
    "fadd faddp fiadd fsub fsubp fisub fsubr fsubrp fisubr ",
    "fmul fmulp fimul fdiv fdivp fidiv fdivr fdivrp fidivr ",
    "fchs fabs fsqrt frndint fprem fprem1 fscale fxtract ",
    // Compare
    "fcom fcomp fcompp ficom ficomp fucom fucomp fucompp ",
    "fcomi fcomip fucomi fucomip ftst fxam ",
    // Transcendental
    "fsin fcos fsincos fptan fpatan f2xm1 fyl2x fyl2xp1 ",
    // Environment / control
    "fnop fwait finit fninit fclex fnclex fstsw fnstsw fstcw fnstcw fldcw ",
    "fstenv fnstenv fldenv fsave fnsave frstor ",
    "fxsave fxrstor ffree fdecstp fincstp ",
);

/// Space-separated **x86-family register** vocabulary installed
/// via `LexAsm`'s `SCI_SETKEYWORDS(2, …)` — class 2 of
/// `asmWordListDesc[]` at `vendor/lexilla/lexers/LexAsm.cxx:80-90`
/// ("Registers"). Drives `SCE_ASM_REGISTER`.
///
/// **Scope: every architecturally-visible register on x86-64 and
/// its 32/16/8-bit predecessors.** General-purpose (`rax` /
/// `eax` / `ax` / `ah` / `al` and the r8..r15 family), instruction
/// pointer / flags in three widths, segment (`cs`/`ds`/`es`/`fs`/
/// `gs`/`ss`), control (`cr0..cr15`), debug (`dr0..dr15`), FPU
/// (`st`/`st0..st7`), MMX (`mm0..mm7`), and SSE/AVX/AVX-512 vector
/// (`xmm0..xmm31`, `ymm0..ymm31`, `zmm0..zmm31`) plus AVX-512 mask
/// (`k0..k7`) and bound (`bnd0..bnd3`).
///
/// **AVX-512 vector register count.** Intel's AVX-512 spec adds 16
/// vector registers on top of AVX's 16, so the full range is 0-31
/// for zmm/ymm/xmm. On non-AVX-512 CPUs registers 16-31 don't
/// physically exist, but the source token still lexes as a
/// register — the assembler is responsible for rejecting them
/// against the target's ISA subset.
pub const ASM_REG_KEYWORDS: &str = concat!(
    // General 8-bit low + high halves
    "al bl cl dl ah bh ch dh ",
    // General 8-bit low-only (need REX prefix in 64-bit mode)
    "spl bpl sil dil ",
    "r8b r9b r10b r11b r12b r13b r14b r15b ",
    // General 16-bit
    "ax bx cx dx si di bp sp ",
    "r8w r9w r10w r11w r12w r13w r14w r15w ",
    // General 32-bit
    "eax ebx ecx edx esi edi ebp esp ",
    "r8d r9d r10d r11d r12d r13d r14d r15d ",
    // General 64-bit
    "rax rbx rcx rdx rsi rdi rbp rsp ",
    "r8 r9 r10 r11 r12 r13 r14 r15 ",
    // Instruction pointer
    "ip eip rip ",
    // Flags
    "flags eflags rflags ",
    // Segment
    "cs ds es fs gs ss ",
    // Control
    "cr0 cr1 cr2 cr3 cr4 cr5 cr6 cr7 cr8 cr9 cr10 cr11 cr12 cr13 cr14 cr15 ",
    // Debug
    "dr0 dr1 dr2 dr3 dr4 dr5 dr6 dr7 dr8 dr9 dr10 dr11 dr12 dr13 dr14 dr15 ",
    // FPU
    "st st0 st1 st2 st3 st4 st5 st6 st7 ",
    // MMX (aliases st0..st7 physically)
    "mm0 mm1 mm2 mm3 mm4 mm5 mm6 mm7 ",
    // SSE/AVX 128-bit
    "xmm0 xmm1 xmm2 xmm3 xmm4 xmm5 xmm6 xmm7 xmm8 xmm9 xmm10 xmm11 xmm12 xmm13 xmm14 xmm15 ",
    "xmm16 xmm17 xmm18 xmm19 xmm20 xmm21 xmm22 xmm23 xmm24 xmm25 xmm26 xmm27 xmm28 xmm29 xmm30 xmm31 ",
    // AVX 256-bit
    "ymm0 ymm1 ymm2 ymm3 ymm4 ymm5 ymm6 ymm7 ymm8 ymm9 ymm10 ymm11 ymm12 ymm13 ymm14 ymm15 ",
    "ymm16 ymm17 ymm18 ymm19 ymm20 ymm21 ymm22 ymm23 ymm24 ymm25 ymm26 ymm27 ymm28 ymm29 ymm30 ymm31 ",
    // AVX-512 512-bit
    "zmm0 zmm1 zmm2 zmm3 zmm4 zmm5 zmm6 zmm7 zmm8 zmm9 zmm10 zmm11 zmm12 zmm13 zmm14 zmm15 ",
    "zmm16 zmm17 zmm18 zmm19 zmm20 zmm21 zmm22 zmm23 zmm24 zmm25 zmm26 zmm27 zmm28 zmm29 zmm30 zmm31 ",
    // AVX-512 mask registers
    "k0 k1 k2 k3 k4 k5 k6 k7 ",
    // MPX bound
    "bnd0 bnd1 bnd2 bnd3",
);

/// Space-separated **assembler directive** vocabulary installed via
/// `LexAsm`'s `SCI_SETKEYWORDS(3, …)` — class 3 of
/// `asmWordListDesc[]` at `vendor/lexilla/lexers/LexAsm.cxx:80-90`
/// ("Directives"). Drives `SCE_ASM_DIRECTIVE`.
///
/// **Scope: the union of MASM, NASM, and GNU-as directive keywords**
/// most likely to appear in x86-family source. Because the lexer
/// runs the same wordlist chain across all three dialects (via
/// `SCLEX_ASM`), collecting them all in one class means a mixed-
/// dialect codebase — or a source that ships with matching NASM
/// and MASM builds — highlights consistently.
///
/// **Special-cased entry: `"comment"`.** `LexAsm` at `:350-356`
/// treats a just-classified DIRECTIVE token equal to literal
/// `"comment"` as MASM's block-comment directive, entering
/// COMMENTDIRECTIVE state until the delimiter reappears. Omitting
/// this entry would break MASM `COMMENT ~ ... ~` block-comment
/// lexing entirely — the block would render as consecutive
/// IDENTIFIERs. Retained here as the first entry.
///
/// **GAS `.`-prefixed directives are stored WITH the leading dot.**
/// `LexAsm.cxx:45-48` (`IsAWordStart`) explicitly admits `.` as a
/// word-start character (alongside `%`, `@`, `$`, `?`); the DEFAULT-
/// state entry at `:414-420` picks `SCE_ASM_IDENTIFIER` on any
/// `IsAWordStart` character (the `.` + digit lookahead branch at
/// `:417` picks `NUMBER` instead, which is why literal `.5` doesn't
/// swallow a dot into an identifier). Result: `.text` scans as the
/// single identifier token `".text"` including the dot, and reaches
/// the inline classifier at `:329-358` — specifically
/// `directive.InList(s)` at `:341` — with the dot present. The
/// wordlist entry MUST include the dot. Parallels
/// [`NSIS_VARIABLES`]'s leading-`$` storage and [`LISP_KEYWORDS_KW`]'s
/// leading-`&` storage.
///
/// **NASM `%`-prefixed preprocessor directives are also stored WITH
/// the leading `%`.** `LexAsm.cxx:45-48` (`IsAWordStart`) admits `%`
/// as a word-start character alongside `.` and `@`, so `%define`
/// scans as a single identifier token `"%define"` — the `%` does NOT
/// terminate the scan. The wordlist entries below (`%define`,
/// `%macro`, `%if`, `%ifndef`, …) therefore preserve the `%` prefix;
/// bare `define` / `macro` / `if` etc. would never match a real NASM
/// preprocessor directive.
pub const ASM_DIRECTIVE_KEYWORDS: &str = concat!(
    // MASM block-comment trigger — MUST be first (see doc-comment)
    "comment ",
    // MASM segment / section
    "segment ends assume model code data const stack ",
    ".model .code .data .data? .const .stack .fardata .fardata? ",
    // MASM procedure / structure. `struc` / `ends` intentionally
    // absent — `ends` is already in the segment section above (MASM
    // uses it for both `SEGMENT`/`ENDS`, `STRUC`/`ENDS`, `STRUCT`/`ENDS`,
    // and `UNION`/`ENDS`); `struc` is included in the NASM section
    // below (NASM's core structure-definition keyword — MASM's `STRUC`
    // is spelled identically so the NASM entry covers both). `equ`
    // deferred to NASM section (same reason: identical spelling across
    // dialects). No `endstruct` / `endunion` entries — MASM closes
    // both blocks with the shared `ENDS` above, not a form-specific
    // `endstruct` / `endunion` (neither of which is a real MASM
    // directive).
    "proc endp struct union ",
    "record typedef textequ label ",
    // MASM linkage / symbol
    "public private external extern extrn global common comm ",
    "includelib include end org align even alias echo option ",
    "invoke ",
    // MASM conditional assembly (case-insensitive: If/ifdef/etc.).
    // Full MASM `IF*` / `ELSEIF*` family per Microsoft's directives
    // reference: IF, IFB, IFDEF, IFDIF, IFDIFI, IFE, IFIDN, IFIDNI,
    // IFNB, IFNDEF, ELSEIF, ELSEIFB, ELSEIFDEF, ELSEIFDIF, ELSEIFDIFI,
    // ELSEIFE, ELSEIFIDN, ELSEIFIDNI, ELSEIFNB, ELSEIFNDEF. Note the
    // negation pattern: `N` appears only in the composite `NB` /
    // `NDEF` suffixes, never as a bare `IFN` / `ELSEIFN` — those
    // don't exist as directives.
    "if ifdef ifndef ifb ifnb ifidn ifidni ifdif ifdifi ife ",
    "elseif elseifdef elseifndef elseifb elseifnb elseifidn elseifidni ",
    "elseifdif elseifdifi elseife else endif ",
    // MASM macro
    "macro endm exitm goto local purge irp irpc rept while endw ",
    // NASM section / declaration (mixed with GAS overlap resolved by unique keys)
    "section bits use16 use32 use64 default cpu warning ",
    "%define %undef %assign %strcat %strlen %substr ",
    "%macro %endmacro %imacro %rmacro %exitmacro %rotate ",
    "%if %ifdef %ifndef %ifnidn %ifidn %ifmacro %ifnmacro %ifctx %ifnctx ",
    "%elif %elifdef %elifndef %else %endif ",
    "%rep %endrep %include %pathsearch %depend ",
    "%push %pop %repl %arg %stacksize %local %line %error %warning %fatal ",
    "%iassign %idefine %ixdefine %xdefine ",
    "resb resw resd resq rest reso resy resz ",
    "db dw dd dq dt do dy dz incbin ",
    "absolute times equ struc endstruc istruc iend at ",
    // GAS pseudo-ops (leading `.`). `.data` intentionally absent
    // — already included in the MASM section above (MASM's
    // `.DATA` and GAS's `.data` are lexically identical so a
    // single entry covers both).
    ".text .bss .rodata .section .previous .subsection ",
    ".globl .global .local .weak .hidden .protected .extern ",
    ".type .size .comm .lcomm .align .balign .balignw .balignl .p2align ",
    ".byte .word .short .int .long .quad .octa .single .double .float ",
    ".string .string8 .string16 .asciz .ascii .space .zero .fill .skip ",
    ".org .set .equ .equiv .eqv ",
    ".rept .endr .macro .endm .purgem .exitm .altmacro .noaltmacro ",
    ".if .ifdef .ifndef .ifb .ifnb .ifc .ifnc .ifeq .ifne .iflt .ifle ",
    ".ifgt .ifge .else .elseif .endif ",
    ".include .incbin .file .line .loc .cfi_startproc .cfi_endproc ",
    ".cfi_offset .cfi_def_cfa .cfi_def_cfa_offset .cfi_def_cfa_register ",
    ".cfi_rel_offset .cfi_adjust_cfa_offset .cfi_restore ",
    ".ident .desc .stabs .stabn .stabd .print .err .fail .warning .error ",
    ".arch .code16 .code32 .code64 .att_syntax .intel_syntax .syntax noprefix prefix ",
);

/// Space-separated **directive-operand qualifier** vocabulary
/// installed via `LexAsm`'s `SCI_SETKEYWORDS(4, …)` — class 4 of
/// `asmWordListDesc[]` at `vendor/lexilla/lexers/LexAsm.cxx:80-90`
/// ("Directive operands"). Drives `SCE_ASM_DIRECTIVEOPERAND`.
///
/// **Scope: size specifiers, distance modifiers, and section /
/// symbol attributes** — the vocabulary that goes NEXT TO
/// directives rather than on their own line. `byte`, `word`,
/// `dword`, `qword`, `tbyte`, `oword`, `xmmword`, `ymmword`,
/// `zmmword` for size prefixes on memory operands (used by MASM,
/// TASM, NASM); `ptr`, `near`, `far`, `short`, `offset`, `seg`
/// for MASM operand-modifier keywords; `flat`, `abs`, `rel` for
/// address-mode selectors; scope / linkage attributes
/// (`readonly`, `readwrite`, `execute`, `discard`, `nopage`,
/// `nocache`, `noshare`, `shared`, `page`, `para`, `dgroup`,
/// `export`) for MASM segment definitions. `alias`, `at`,
/// `common`, `private`, `public` are ALSO MASM segment attributes
/// but their primary use is as top-level directives and they are
/// routed to [`ASM_DIRECTIVE_KEYWORDS`] (class 3) — see the inline
/// comment on the wordlist body below.
pub const ASM_DIRECTIVE_OP_KEYWORDS: &str = concat!(
    // Size specifiers
    "byte word dword qword tbyte tword fword oword xmmword ymmword zmmword ",
    // Distance modifiers
    "ptr near far short offset seg flat abs rel ",
    // Segment attributes (MASM segment definition). `alias`, `at`,
    // `common`, `private`, `public` are ALSO MASM segment-attribute
    // keywords but their primary use is as top-level directives —
    // routed via ASM_DIRECTIVE_KEYWORDS so `public foo` /
    // `common bar` at column-0 highlight as directives; the
    // attribute-form (`.segment name public`) paints identically.
    "readonly readwrite execute discard nopage nocache noshare shared ",
    "page para dgroup ",
    // Scope-only extras
    "export ",
    // Type qualifiers
    "signed unsigned ",
);

/// Space-separated **extended-instruction** vocabulary installed
/// via `LexAsm`'s `SCI_SETKEYWORDS(5, …)` — class 5 of
/// `asmWordListDesc[]` at `vendor/lexilla/lexers/LexAsm.cxx:80-90`
/// ("Extended instructions"). Drives `SCE_ASM_EXTINSTRUCTION`.
///
/// **Scope: SIMD (MMX / SSE / SSE2..SSE4.2 / AVX / AVX2 / AVX-512
/// F+VL+DQ+BW+CD / FMA3 / AES-NI / PCLMULQDQ / SHA / 3DNow!)** —
/// the vector-instruction family. This class exists specifically
/// because vectorised code visually reads different from scalar
/// integer/FP code, and users tuning SIMD-heavy inner loops want
/// the SIMD lines to pop out.
///
/// **Only distinct mnemonics.** MMX-and-SSE overlap on some
/// mnemonics (`emms`, `movd`, `movq` MMX vs `movq` SSE2 xmm form)
/// — the mnemonic appears once regardless.
pub const ASM_EXT_KEYWORDS: &str = concat!(
    // MMX
    "emms movd movq paddb paddw paddd paddq paddsb paddsw paddusb paddusw ",
    "psubb psubw psubd psubq psubsb psubsw psubusb psubusw ",
    "pmullw pmulhw pmulhuw pmuludq pmaddwd ",
    "pand pandn por pxor ",
    "pcmpeqb pcmpeqw pcmpeqd pcmpgtb pcmpgtw pcmpgtd ",
    "psllw pslld psllq psrlw psrld psrlq psraw psrad ",
    "packsswb packssdw packuswb ",
    "punpckhbw punpckhwd punpckhdq punpcklbw punpcklwd punpckldq ",
    "movntq maskmovq pavgb pavgw psadbw ",
    // SSE (scalar + packed single)
    "movss movaps movups movhps movlps movhlps movlhps movmskps ",
    "addss addps subss subps mulss mulps divss divps sqrtss sqrtps ",
    "rcpss rcpps rsqrtss rsqrtps minss minps maxss maxps ",
    "cmpss cmpps comiss ucomiss ",
    "andps andnps orps xorps unpckhps unpcklps shufps ",
    "cvtsi2ss cvtss2si cvttss2si cvtps2pi cvttps2pi cvtpi2ps ",
    "ldmxcsr stmxcsr sfence prefetchnta prefetcht0 prefetcht1 prefetcht2 ",
    "movntps ",
    // SSE2. `movsd` / `cmpsd` / `movnti` / `pause` deliberately
    // absent — resolved as CPU (see ASM_CPU_KEYWORDS notes on
    // string-op-vs-SIMD mnemonic overload for movsd/cmpsd; movnti
    // is scalar non-temporal store; pause is a spin-loop hint).
    "movapd movupd movhpd movlpd movdqa movdqu movdq2q movq2dq ",
    "addsd addpd subsd subpd mulsd mulpd divsd divpd sqrtsd sqrtpd ",
    "minsd minpd maxsd maxpd cmppd comisd ucomisd ",
    "andpd andnpd orpd xorpd unpckhpd unpcklpd shufpd ",
    "cvtsi2sd cvtsd2si cvttsd2si cvtsd2ss cvtss2sd ",
    "cvtps2pd cvtpd2ps cvtdq2ps cvtdq2pd cvtps2dq cvtpd2dq cvttps2dq cvttpd2dq ",
    "movntdq movntpd maskmovdqu lfence mfence ",
    // Integer add/subtract/shift mnemonics that MMX and SSE2
    // share (encoding differs by operand register class — mm* vs
    // xmm* — but assemblers use the same mnemonic; LexAsm sees
    // only the token). Already declared in the MMX section above
    // — omitted here.
    "pshuflw pshufhw pshufd pslldq psrldq ",
    // SSE3. `fisttp` absent — routed via ASM_FPU_KEYWORDS as x87
    // FPU truncate-store (SSE3-introduced but FPU-native).
    "addsubps addsubpd haddps haddpd hsubps hsubpd movsldup movshdup movddup ",
    "lddqu monitor mwait ",
    // SSSE3
    "phaddw phaddd phaddsw phsubw phsubd phsubsw pmaddubsw pmulhrsw ",
    "pshufb psignb psignw psignd pabsb pabsw pabsd palignr ",
    // SSE4.1
    "blendps blendvps blendpd blendvpd pblendw pblendvb ",
    "dpps dppd insertps extractps roundss roundsd roundps roundpd ",
    "mpsadbw pmaxsb pmaxud pmaxuw pminsb pminud pminuw ",
    "pmovsxbw pmovsxbd pmovsxbq pmovsxwd pmovsxwq pmovsxdq ",
    "pmovzxbw pmovzxbd pmovzxbq pmovzxwd pmovzxwq pmovzxdq ",
    "pmulld pmuldq ptest ",
    "pinsrb pinsrd pinsrq pextrb pextrw pextrd pextrq ",
    "packusdw phminposuw ",
    // SSE4.2. `popcnt` deliberately absent — routed via
    // ASM_CPU_KEYWORDS as it operates on scalar integer domain
    // and reads as a general-purpose instruction to most users.
    "crc32 pcmpestri pcmpestrm pcmpistri pcmpistrm pcmpgtq ",
    // AES-NI + PCLMULQDQ
    "aesdec aesdeclast aesenc aesenclast aesimc aeskeygenassist ",
    "pclmulqdq pclmulhqhqdq pclmulhqlqdq pclmullqhqdq pclmullqlqdq ",
    // SHA
    "sha1rnds4 sha1nexte sha1msg1 sha1msg2 sha256rnds2 sha256msg1 sha256msg2 ",
    // AVX (VEX-encoded, most SSE ops get a `v` prefix — abbreviated set here)
    "vmovss vmovsd vmovaps vmovapd vmovups vmovupd vmovdqa vmovdqu ",
    "vaddss vaddsd vaddps vaddpd vsubss vsubsd vsubps vsubpd ",
    "vmulss vmulsd vmulps vmulpd vdivss vdivsd vdivps vdivpd ",
    "vsqrtss vsqrtsd vsqrtps vsqrtpd vrcpss vrcpps vrsqrtss vrsqrtps ",
    "vminss vminsd vminps vminpd vmaxss vmaxsd vmaxps vmaxpd ",
    "vcmpss vcmpsd vcmpps vcmppd vcomiss vcomisd vucomiss vucomisd ",
    "vandps vandpd vorps vorpd vxorps vxorpd vandnps vandnpd ",
    "vshufps vshufpd vunpckhps vunpckhpd vunpcklps vunpcklpd vblendps vblendpd ",
    "vblendvps vblendvpd vinsertps vextractps ",
    "vbroadcastss vbroadcastsd vbroadcastf128 vinsertf128 vextractf128 ",
    "vperm2f128 vpermilps vpermilpd vzeroall vzeroupper ",
    // FMA3
    "vfmadd132ps vfmadd213ps vfmadd231ps vfmadd132pd vfmadd213pd vfmadd231pd ",
    "vfmadd132ss vfmadd213ss vfmadd231ss vfmadd132sd vfmadd213sd vfmadd231sd ",
    "vfmsub132ps vfmsub213ps vfmsub231ps vfmsub132pd vfmsub213pd vfmsub231pd ",
    "vfmsub132ss vfmsub213ss vfmsub231ss vfmsub132sd vfmsub213sd vfmsub231sd ",
    "vfnmadd132ps vfnmadd213ps vfnmadd231ps vfnmsub132ps vfnmsub213ps vfnmsub231ps ",
    // AVX-512 core (F/CD/ER/PF + VL/DQ/BW + IFMA + VBMI — abbreviated)
    "vpaddb vpaddw vpaddd vpaddq vpsubb vpsubw vpsubd vpsubq ",
    "vpmullw vpmulld vpmullq vpmulhw vpmulhrsw vpmuldq vpmuludq ",
    "vpandd vpandq vpandnd vpandnq vpord vporq vpxord vpxorq ",
    "vpermd vpermq vpermps vpermpd vpermi2ps vpermi2pd vpermt2ps vpermt2pd ",
    "vbroadcasti32x4 vbroadcasti64x4 vbroadcastf32x4 vbroadcastf64x4 ",
    "vextracti32x4 vextracti64x4 vextractf32x4 vextractf64x4 ",
    "vinserti32x4 vinserti64x4 vinsertf32x4 vinsertf64x4 ",
    "vpternlogd vpternlogq vptestmd vptestmq vptestnmd vptestnmq ",
    "vscatterdps vscatterqps vscatterdpd vscatterqpd ",
    "vgatherdps vgatherqps vgatherdpd vgatherqpd ",
    "vpcompressd vpcompressq vcompressps vcompresspd ",
    "vpexpandd vpexpandq vexpandps vexpandpd ",
    "kmovb kmovw kmovd kmovq kandb kandw kandd kandq korb korw kord korq ",
    "kxorb kxorw kxord kxorq knotb knotw knotd knotq ",
    "kshiftlb kshiftlw kshiftld kshiftlq kshiftrb kshiftrw kshiftrd kshiftrq ",
    "kortestb kortestw kortestd kortestq ktestb ktestw ktestd ktestq ",
    // 3DNow!
    "femms pfadd pfsub pfsubr pfmul pfdiv pfrsqrt pfrcp pfmin pfmax ",
    "pfcmpge pfcmpgt pfcmpeq pfacc pfnacc pfpnacc ",
    "pi2fw pi2fd pf2iw pf2id pmulhrw pavgusb pswapd prefetch prefetchw ",
    // Intel Xeon Phi / Knights Landing prefetch hint (AVX-512-adjacent).
    "prefetchwt1",
);

/// Space-separated R7RS Scheme reserved-word vocabulary installed via
/// the shared `lisp` Lexilla lexer's `SCI_SETKEYWORDS(0, …)` — class 0
/// of `lispWordListDesc[]` at `vendor/lexilla/lexers/LexLisp.cxx:280-284`
/// ("Functions and special operators"). Drives `SCE_LISP_KEYWORD` per
/// `LexLisp.cxx:64-65`.
///
/// **Two-class contract with [`SCHEME_KEYWORDS_KW`] on class 1.** No
/// cross-class duplicates — verified pairwise by a `HashSet`
/// intersection guard in `scheme_reuses_lexlisp_theme_with_r7rs_wordlists`.
/// The lexer's first-match-wins chain at `LexLisp.cxx:64-68` would
/// silently demote any duplicate class-1 entry to `SCE_LISP_KEYWORD`.
///
/// **Case-sensitive.** `LexLisp` `classifyWordLisp` at
/// `LexLisp.cxx:50-75` does raw byte copies with NO case folding
/// (no `MakeLowerCase` / `tolower` / `GetCurrentLowered` on the
/// wordlist-match path); `WordList::InList` does byte-equality.
/// R6RS §4.2 and R7RS §2.1 mandate case-sensitivity for identifiers
/// (reversing R5RS's case-insensitivity), so canonical lowercase is
/// correct — every R7RS report code sample and every modern
/// implementation ships identifiers in lowercase.
///
/// **`:`-prefix exclusion.** Keyword-argument symbols (`:test`,
/// `:key`) enter `SCE_LISP_SYMBOL` via the DEFAULT-state branch at
/// `LexLisp.cxx:107-109` and NEVER reach `classifyWordLisp` — any
/// `:`-prefixed wordlist entry would be unreachable spec noise.
/// Same guard as [`LISP_KEYWORDS`].
///
/// **Source of vocabulary.** R7RS-small §7 formal syntax + §6.1-6.14
/// procedure indices. Cross-checked against Notepad++'s
/// `langs.model.xml` `<Language name="scheme">` `instre1` for
/// default-set parity; extended to full R7RS (Notepad++'s list is
/// R5RS-flavoured — no `define-record-type`, `guard`, `parameterize`,
/// `when` / `unless`, `case-lambda`, bytevectors, R7RS I/O, library
/// forms). SRFI-1 higher-order idioms (`filter`, `fold`, `fold-left`,
/// `fold-right`, `reduce`) also included — de facto standard even in
/// R7RS-small codebases. No content copied from Notepad++ per
/// CLAUDE.md.
///
/// **Divergence from Common Lisp.** This list carries R7RS canon
/// (`define`, `letrec`, `syntax-rules`, `set!`, `call/cc`,
/// `dynamic-wind`, …). The Common Lisp counterparts (`defun`,
/// `labels`, `rplaca`, `atom`, `null`, `eq`, `equal`) live in
/// [`LISP_KEYWORDS`] and do NOT belong here. The class-0 archetype
/// includes `set!` — a *special form* per R7RS §4.1.6 (Assignments),
/// grouped with the binding-shape forms (`define` / `let` / `letrec`),
/// NOT with the `!`-suffix mutator procedures (`set-car!`,
/// `vector-set!`) which are class 1. The `!`-ending on `set!` is
/// coincidental to its syntactic role.
pub const SCHEME_KEYWORDS: &str = concat!(
    "* + - / < <= = => > >= abs acos and angle append apply asin ",
    "assoc assq assv atan begin ",
    "bytevector bytevector-append bytevector-copy bytevector-length ",
    "bytevector-u8-ref ",
    "caaaar caaadr caaar caadar caaddr caadr caar cadaar cadadr ",
    "cadar caddar cadddr caddr cadr ",
    "call-with-current-continuation call-with-input-file ",
    "call-with-output-file call-with-port call-with-values call/cc ",
    "car case case-lambda ",
    "cdaaar cdaadr cdaar cdadar cdaddr cdadr cdar cddaar cddadr ",
    "cddar cdddar cddddr cdddr cddr cdr ",
    "ceiling char->integer char-downcase char-foldcase char-upcase ",
    "close-input-port close-output-port close-port command-line ",
    "cond cond-expand cons cos ",
    "current-error-port current-input-port current-output-port ",
    "define define-library define-record-type define-syntax ",
    "define-values delay delay-force denominator display do ",
    "dynamic-wind else emergency-exit environment eof-object ",
    "error error-object-irritants error-object-message eval exact ",
    "exact->inexact exact-integer-sqrt exp export expt features filter ",
    "floor floor-quotient floor-remainder floor/ flush-output-port ",
    "fold fold-left fold-right for-each force gcd ",
    "get-output-string guard if imag-part import include include-ci ",
    "inexact inexact->exact integer->char interaction-environment ",
    "lambda lcm length let let* let*-values let-syntax let-values ",
    "letrec letrec* letrec-syntax library ",
    "list list->string list->vector list-copy list-ref list-tail ",
    "load log magnitude ",
    "make-bytevector make-list make-polar make-promise ",
    "make-rectangular make-string make-vector map max member memq ",
    "memv min modulo newline not null-environment number->string ",
    "numerator open-input-file open-input-string open-output-file ",
    "open-output-string or parameterize peek-char peek-u8 ",
    "quasiquote quote quotient raise raise-continuable rationalize ",
    "read read-bytevector read-char read-line read-string read-u8 ",
    "real-part reduce remainder reverse round ",
    "scheme-report-environment set! sin sqrt square ",
    "string string->list string->number string->symbol string->utf8 ",
    "string->vector string-append string-copy string-downcase ",
    "string-for-each string-length string-map string-ref string-upcase ",
    "substring symbol->string syntax-rules tan ",
    "truncate truncate-quotient ",
    "truncate-remainder truncate/ unless unquote unquote-splicing ",
    "utf8->string values vector vector->list vector->string ",
    "vector-append vector-copy vector-for-each vector-length vector-map ",
    "vector-ref when with-exception-handler with-input-from-file ",
    "with-output-to-file write write-bytevector write-char ",
    "write-string write-u8",
);

/// Space-separated R7RS Scheme predicate + mutator vocabulary installed
/// via the shared `lisp` Lexilla lexer's `SCI_SETKEYWORDS(1, …)` —
/// class 1 of `lispWordListDesc[]` at `LexLisp.cxx:280-284`
/// ("Keywords"). Drives `SCE_LISP_KEYWORD_KW` per `LexLisp.cxx:66-67`
/// → Keyword2 in `LISP_STYLES`.
///
/// **Scope: `?`-suffix predicates + `!`-suffix destructive procedures
/// ONLY.** This is Scheme's *semantic* sigil contract (R7RS §1.3.5):
/// identifiers ending in `?` are predicates that return a boolean;
/// identifiers ending in `!` mutate their arguments. Structural
/// parallel to [`LISP_KEYWORDS_KW`]'s syntactic leading-`&` contract
/// on class 1 — both slots reserve the sigil-tagged archetype for the
/// Keyword2 colour.
///
/// **`set!` is NOT here.** The trailing `!` is coincidental to `set!`'s
/// R7RS §4.1.6 role as an *assignment special form* — a syntactic
/// binder, not a data mutator like `set-car!` / `vector-set!` /
/// `string-fill!`. `set!` lives in [`SCHEME_KEYWORDS`] class 0 with
/// the other binding-shape forms.
///
/// **`#t` / `#f` / `#true` / `#false` are NOT here.** Leading `#`
/// enters `SCE_LISP_MACRO_DISPATCH` at `LexLisp.cxx:106` (private
/// state) and remaps to `SCE_LISP_SPECIAL` on emission. Never reaches
/// `classifyWordLisp` — the wordlist path never sees these tokens.
///
/// **No cross-class duplicates with [`SCHEME_KEYWORDS`].** Verified by
/// the same `HashSet` intersection guard. Adjacent-name pairs like
/// `string-copy` (class 0) vs `string-copy!` (class 1),
/// `vector-copy` vs `vector-copy!`, and `char-upcase` / `char-downcase`
/// (class 0, char transformers) vs `char-upper-case?` /
/// `char-lower-case?` (class 1, char predicates) all resolve cleanly
/// to opposite classes.
///
/// Notepad++'s stock `langs.model.xml` ships class 1 empty for Scheme
/// (all predicates + mutators dumped into `instre1`). This class-1
/// population is a strictly-additive visual enhancement — nothing
/// Notepad++ paints changes; tokens Notepad++ paints as class-0
/// KEYWORD get promoted to `KEYWORD_KW`.
pub const SCHEME_KEYWORDS_KW: &str = concat!(
    "binary-port? boolean=? boolean? bytevector? ",
    "char-alphabetic? char-ci<=? char-ci<? char-ci=? char-ci>=? ",
    "char-ci>? char-lower-case? char-numeric? char-ready? ",
    "char-upper-case? char-whitespace? ",
    "char<=? char<? char=? char>=? char>? char? ",
    "complex? eof-object? eq? equal? eqv? error-object? ",
    "even? exact-integer? exact? file-error? file-exists? finite? ",
    "infinite? inexact? input-port-open? ",
    "input-port? integer? list? nan? negative? null? number? odd? ",
    "output-port-open? output-port? pair? port? positive? procedure? ",
    "promise? rational? read-error? real? ",
    "string-ci<=? string-ci<? string-ci=? string-ci>=? string-ci>? ",
    "string<=? string<? string=? string>=? string>? string? ",
    "symbol=? symbol? textual-port? vector? zero? ",
    "bytevector-copy! bytevector-u8-set! set-car! set-cdr! ",
    "string-copy! string-fill! string-set! ",
    "vector-copy! vector-fill! vector-set!",
);

/// Space-separated Python 3 reserved-word vocabulary installed via
/// `LexPython`'s `SCI_SETKEYWORDS(0, ...)` — class 0 of
/// `pythonWordListDesc[]`. Drives `SCE_P_WORD` (mapped to Keyword
/// bold blue).
///
/// **Case-sensitive lexer.** `LexPython.cxx:671` calls
/// `keywords.InList(identifier)` with no case folding — confirmed by
/// zero matches for `tolower` / `MakeLowerCase` / `GetCurrentLowered`
/// in the source. Wordlists must store source-canonical casing.
/// Python language semantics: `True`, `False`, `None` are spelled
/// with leading capitals; every other reserved word is lowercase.
/// Same byte-exact contract as [`PERL_KEYWORDS`].
///
/// **Two-class wordlist with [`PYTHON_KEYWORDS_2`].** Class 0 holds
/// the 37 reserved + soft-keyword tokens (exactly `keyword.kwlist`
/// from Python's `keyword` module, plus `match` / `case` from
/// `keyword.softkwlist`). Class 1 holds 270 built-in identifiers
/// (functions, exception types, conventional names like `self` /
/// `cls`, sentinel literals, dunder methods). Lexilla checks class 0
/// FIRST (line 671), so a collision would silently demote class 1
/// entries — pinned no-overlap structurally in the Python theme
/// test.
///
/// **`True` / `False` / `None` placement.** Python 3 makes these
/// hard reserved words (`True = 5` is a `SyntaxError`, unlike Python 2
/// where they were builtins). Code++ routes them through class 0 so
/// they render Keyword-bold alongside `def` / `class` / `if`,
/// matching their reserved-word status. Notepad++ historically
/// placed them in WORD2 (built-in slot) for backward compatibility
/// with Python 2; Code++ deliberately diverges to honour Python 3
/// language semantics.
///
/// **`match` / `case` soft keywords.** Python 3.10+ PEP 634 makes
/// these reserved ONLY in pattern-matching position (`match value:`
/// / `case 1:`); elsewhere (`match = 1`, `obj.match()`) they're
/// regular identifiers. LexPython.cxx:258-289 (`IsMatchOrCaseIdentifier`)
/// vetoes the wordlist hit in non-pattern context — the token falls
/// through to `SCE_P_IDENTIFIER`. Installing them in class 0 is
/// correct and safe; the lexer disambiguates.
///
/// Sourced from Python 3.12's `keyword.kwlist` + `keyword.softkwlist`.
/// Adversarial-verifier ACCEPT on correctness + completeness +
/// format (all three lenses agreed, zero corrections required).
pub const PYTHON_KEYWORDS: &str = concat!(
    // Python 3 reserved literals (capitalised per source casing)
    "False None True ",
    // Boolean / membership / identity operators
    "and as assert async await ",
    // Control flow + iteration
    "break class continue def del elif else except finally for from ",
    // Declaration scopes + control flow
    "global if import in is lambda nonlocal not or pass raise return ",
    // Exception + iteration + context-manager + concurrency
    "try while with yield ",
    // Python 3.10+ pattern matching (soft keywords;
    // `IsMatchOrCaseIdentifier` disambiguates non-pattern context)
    "match case",
);

/// Space-separated Python built-in identifier vocabulary installed
/// via `LexPython`'s `SCI_SETKEYWORDS(1, ...)` — class 1 of
/// `pythonWordListDesc[]`. Drives `SCE_P_WORD2` (mapped to Keyword2
/// steel-blue).
///
/// **Case preserved per Python source convention.** CamelCase
/// exception classes (`Exception`, `ValueError`), dunder names with
/// double underscores intact (`__init__`, `__repr__`), lowercase
/// built-in functions (`print`, `len`, `range`), conventional
/// parameter names (`self`, `cls`).
///
/// Bundles four sub-categories:
///   * **Built-in functions** (~70 entries from Python `builtins`
///     module): `abs` / `all` / `any` / `bin` / `bool` / `bytes` /
///     `callable` / `chr` / `dict` / `enumerate` / `filter` /
///     `float` / `getattr` / `hasattr` / `hash` / `id` / `input` /
///     `int` / `isinstance` / `iter` / `len` / `list` / `map` /
///     `max` / `min` / `next` / `object` / `open` / `ord` /
///     `print` / `property` / `range` / `repr` / `reversed` /
///     `round` / `set` / `sorted` / `str` / `sum` / `super` /
///     `tuple` / `type` / `vars` / `zip` — plus modern additions
///     `aiter` / `anext` / `breakpoint`.
///   * **Exception + warning hierarchy** (60+ entries): everything
///     from `BaseException` / `Exception` down through
///     `ValueError` / `TypeError` / `KeyError` / `IndexError` /
///     `OSError` and the full `FileNotFoundError` /
///     `PermissionError` / `ConnectionError` family + the modern
///     `Warning` subclasses (`DeprecationWarning` /
///     `RuntimeWarning` / etc.). Includes legacy aliases
///     (`IOError` / `EnvironmentError` = `OSError` aliases still
///     importable in Python 3, `WindowsError` Windows-only).
///   * **Sentinels + module dunders**: `Ellipsis` /
///     `NotImplemented` (canonical sentinels) + `__debug__` /
///     `__name__` / `__doc__` / `__file__` / `__loader__` /
///     `__package__` / `__spec__` / `__import__` /
///     `__build_class__` (module-level dunders).
///   * **Data-model dunders + conventional names** (110+
///     entries) covering the full Python data-model protocol:
///     `__init__`, `__new__`, `__del__`, `__repr__`, `__str__`,
///     comparison (`__eq__`, `__hash__`, `__lt__`, `__le__`,
///     `__gt__`, `__ge__`, `__ne__`, `__bool__`), attribute
///     access (`__getattr__`, `__setattr__`, `__call__`,
///     `__len__`, `__getitem__`, `__setitem__`, `__iter__`,
///     `__next__`), context managers sync + async (`__enter__`,
///     `__exit__`, `__aiter__`, `__anext__`, `__aenter__`,
///     `__aexit__`, `__await__`), the arithmetic + reflected +
///     inplace cluster (`__add__`, `__radd__`, `__iadd__` and
///     siblings for sub, mul, truediv, floordiv, mod, divmod,
///     pow, lshift, rshift, and, xor, or) — plus conventional
///     `self` and `cls` parameter names that every style guide
///     highlights despite not being reserved.
///
/// **`self` / `cls` rationale.** Not reserved (`def foo(this,
/// that)` is legal Python), but every style guide and IDE
/// highlights them. Class-1 placement matches Notepad++'s WORD2
/// convention and gives them the same Keyword2 accent as other
/// built-in identifiers without claiming reserved-word status.
///
/// **No cross-class duplicates with [`PYTHON_KEYWORDS`].** Verified
/// by `HashSet` intersection before commit AND structurally pinned
/// by the Python theme test. `True` / `False` / `None` are class 0
/// ONLY; `self` / `cls` are class 1 ONLY; `match` / `case` are
/// class 0 ONLY.
///
/// Sourced from `dir(builtins)` (Python 3.12), Notepad++'s
/// shipped `langs.model.xml` `<Language name="python">` `instre2`
/// list, and the full Python data-model documentation. Adversarial-
/// verifier ACCEPT on correctness + completeness + format.
///
/// **Minor sourcing nits, documented for the next maintainer:**
///   * `exit` and `quit` are `_sitebuiltins.Quitter` objects
///     injected by Python's `site` module at interpreter startup,
///     not technically members of `builtins` proper (try
///     `python3 -S -c 'exit'` — raises `NameError`). They're
///     included because they're universally available outside the
///     `-S` flag and Notepad++ ships them; documentation note only,
///     no behavioural impact.
///   * Python 3.13+ adds `type` to `keyword.softkwlist` (PEP 695
///     type alias soft keyword). Deliberately omitted from class 0
///     because `type` is already class 1 as the metaclass built-in,
///     and `LexPython` has no `IsTypeIdentifier` disambiguation
///     guard analogous to `IsMatchOrCaseIdentifier` — class-0
///     placement would over-aggressively style `type(x)` and
///     `isinstance(x, type)` as a Keyword. Promote on a future
///     Lexilla update that adds the disambiguation guard.
pub const PYTHON_KEYWORDS_2: &str = concat!(
    // Built-in functions (lowercase, ~70 entries)
    "abs aiter all anext any ascii bin bool breakpoint bytearray ",
    "bytes callable chr classmethod compile complex delattr dict ",
    "dir divmod enumerate eval exec exit filter float format ",
    "frozenset getattr globals hasattr hash help hex id input int ",
    "isinstance issubclass iter len list locals map max memoryview ",
    "min next object oct open ord pow print property quit range ",
    "repr reversed round set setattr slice sorted staticmethod str ",
    "sum super tuple type vars zip ",
    // Exception + warning hierarchy (CamelCase, ~60 entries)
    "ArithmeticError AssertionError AttributeError BaseException ",
    "BlockingIOError BrokenPipeError BufferError BytesWarning ",
    "ChildProcessError ConnectionAbortedError ConnectionError ",
    "ConnectionRefusedError ConnectionResetError DeprecationWarning ",
    "EOFError EncodingWarning EnvironmentError Exception ",
    "FileExistsError FileNotFoundError FloatingPointError ",
    "FutureWarning GeneratorExit IOError ImportError ImportWarning ",
    "IndentationError IndexError InterruptedError IsADirectoryError ",
    "KeyError KeyboardInterrupt LookupError MemoryError ",
    "ModuleNotFoundError NameError NotADirectoryError ",
    "NotImplementedError OSError OverflowError ",
    "PendingDeprecationWarning PermissionError ProcessLookupError ",
    "RecursionError ReferenceError ResourceWarning RuntimeError ",
    "RuntimeWarning StopAsyncIteration StopIteration SyntaxError ",
    "SyntaxWarning SystemError SystemExit TabError TimeoutError ",
    "TypeError UnboundLocalError UnicodeDecodeError ",
    "UnicodeEncodeError UnicodeError UnicodeTranslateError ",
    "UnicodeWarning UserWarning ValueError Warning WindowsError ",
    "ZeroDivisionError ",
    // Sentinels + module dunders
    "Ellipsis NotImplemented ",
    "__debug__ __build_class__ __doc__ __import__ __loader__ ",
    "__name__ __package__ __spec__ ",
    // Conventional parameter names
    "self cls ",
    // Data-model dunders — lifecycle + representation
    "__init__ __new__ __del__ __repr__ __str__ __bytes__ __format__ ",
    // Comparison
    "__lt__ __le__ __eq__ __ne__ __gt__ __ge__ __hash__ __bool__ ",
    // Attribute access
    "__getattr__ __getattribute__ __setattr__ __delattr__ __dir__ ",
    // Descriptors
    "__get__ __set__ __delete__ __set_name__ ",
    // Class introspection / metaclass
    "__init_subclass__ __class_getitem__ __slots__ __mro_entries__ ",
    // Callable + container protocol
    "__call__ __len__ __length_hint__ __getitem__ __setitem__ ",
    "__delitem__ __missing__ __iter__ __reversed__ __contains__ ",
    "__next__ ",
    // Arithmetic (regular + reflected + inplace)
    "__add__ __radd__ __iadd__ __sub__ __rsub__ __isub__ ",
    "__mul__ __rmul__ __imul__ __truediv__ __rtruediv__ __itruediv__ ",
    "__floordiv__ __rfloordiv__ __ifloordiv__ __mod__ __rmod__ ",
    "__imod__ __divmod__ __rdivmod__ __pow__ __rpow__ __ipow__ ",
    // Bit-shift + bitwise
    "__lshift__ __rlshift__ __ilshift__ __rshift__ __rrshift__ ",
    "__irshift__ __and__ __rand__ __iand__ __xor__ __rxor__ ",
    "__ixor__ __or__ __ror__ __ior__ ",
    // Unary + numeric conversion
    "__neg__ __pos__ __abs__ __invert__ __complex__ __int__ ",
    "__float__ __index__ __round__ __trunc__ __floor__ __ceil__ ",
    // Context managers (sync + async)
    "__enter__ __exit__ __aiter__ __anext__ __aenter__ __aexit__ ",
    "__await__ ",
    // Pickle / copy
    "__copy__ __deepcopy__ __reduce__ __reduce_ex__ __getstate__ ",
    "__setstate__ __getnewargs__ __getnewargs_ex__ ",
    // Class hooks
    "__subclasshook__ __instancecheck__ __subclasscheck__ ",
    // Object introspection attributes
    "__class__ __dict__ __module__ __qualname__ __weakref__ ",
    "__annotations__ __all__ __file__ __path__ __version__ __author__",
);

/// Space-separated Bash / POSIX-shell reserved-word + builtin
/// vocabulary installed via `LexBash`'s `SCI_SETKEYWORDS(0, ...)`
/// — the only class accepted by `LexerBash::WordListSet` per
/// `vendor/lexilla/lexers/LexBash.cxx:558-572`. Drives `SCE_SH_WORD`
/// (mapped to Keyword bold blue).
///
/// **Single-class wordlist surface.** `bashWordListDesc[]` at
/// `LexBash.cxx:205-208` declares ONE named slot, `"Keywords"`,
/// terminated by `nullptr`. Unlike Lua (2 classes), Python (2
/// classes), or SQL (5 classes), Bash has no second / third
/// wordlist — reserved words and builtins necessarily share class 0.
/// There is no `BASH_KEYWORDS_2`.
///
/// **Case-sensitive byte-exact match.** `LexBash.cxx:727` calls
/// `keywords.InList(s)` against the raw `sc.GetCurrent(s, ...)`
/// buffer — no `MakeLowerCase` / `GetCurrentLowered` anywhere in
/// the lexer. Confirmed by grepping the full source. Bash language
/// semantics: every reserved word and builtin is lowercase. An
/// uppercase entry below would never match.
///
/// **Command-Start position only.** `keywords.InList(s)` fires
/// only when `cmdState == CmdState::Start` AND `keywordEnds` per
/// `LexBash.cxx:726-728`. This means user-supplied keywords
/// highlight ONLY when they appear as the first word of a command
/// (matching how real Bash builtins / reserved words behave) —
/// `echo "foo"` styles `echo` as `SCE_SH_WORD`; `bar echo "foo"`
/// where `echo` is a sub-command argument styles it as
/// `SCE_SH_IDENTIFIER`. Same Start-position gate applies to the
/// hard-wired structural sets.
///
/// **Structural reserved words handled by `bashStruct` — NOT
/// duplicated here.** `LexBash.cxx:492` populates `bashStruct =
/// "if elif fi while until else then do done esac eval"` and
/// `:493` populates `bashStruct_in = "for case select"`; both
/// are matched at `:706, :713` independently of the user
/// wordlist. Adding the control-flow tokens (`if`, `then`, `fi`,
/// `while`, `for`, `case`, `select`, `in`, …) to this list would
/// be no-op spec noise — the lexer would hit the hard-wired set
/// first. The list below covers builtins (`echo`, `printf`,
/// `read`, …) and reserved words NOT in `bashStruct` that the
/// word-start gate at `LexBash.cxx:575` admits (`function`,
/// `time`, `coproc`). The `!` negation token and `[` / `[[` /
/// `]]` test-command brackets are deliberately NOT in this list
/// — `setWordStart = setAlpha + "_"` at `LexBash.cxx:575`
/// rejects them before keyword classification can fire (they
/// route to `SCE_SH_OPERATOR` via `setBashOperator` at `:580`),
/// so adding them would be unreachable spec noise.
///
/// **Sourcing.** Bash Reference Manual §3.1 ("Shell Syntax") +
/// §4.1 ("Bourne Shell Builtins") + §4.2 ("Bash Builtin
/// Commands"). Cross-referenced against N++'s shipped
/// `langs.model.xml` `<Language name="bash">` `instre1` list for
/// default-set parity. The N++ file is referenced for parity
/// inspection only — no content copied from it (per the
/// CLAUDE.md "no code from Notepad++" rule); the canonical
/// source for every entry below is the Bash Reference Manual.
pub const BASH_KEYWORDS: &str = concat!(
    // Reserved tokens not in `bashStruct` and accepted by the
    // word-start gate: `function` / `coproc` declaration,
    // `time` pipeline timing. (`select` is already in
    // `bashStruct_in`; `in` is matched by the `CmdState::Word`
    // transition at LexBash.cxx:688-690.)
    "coproc function time ",
    // Declaration + scope builtins
    "alias declare export local readonly typeset unalias unset ",
    // I/O + variable manipulation builtins
    "echo getopts let mapfile printf read readarray ",
    // Process / job control builtins
    "bg disown exec exit fg jobs kill suspend wait ",
    // Navigation + directory stack builtins
    "cd dirs popd pushd pwd ",
    // Shell-mode + option builtins
    "enable set shift shopt umask ulimit ",
    // Conditional + history + completion builtins
    "bind builtin caller command complete compgen compopt ",
    // Test / type / source / trap / return / break / continue.
    // `test` is deliberately NOT in this list — `LexBash.cxx:699`
    // matches it via a hard-wired `strcmp` (separate from the
    // `bashStruct` set noted above) that fires before
    // `keywords.InList` at `:726-728` is consulted, so an entry
    // here would be unreachable spec noise. `times` (the shell
    // builtin printing accumulated process times) is a real
    // builtin — kept.
    "break continue false fc hash help history logout return ",
    "source times trap true type ",
);

/// Space-separated NSIS instruction / `!`-directive vocabulary
/// installed via `LexNsis`'s `SCI_SETKEYWORDS(0, ...)` — class 0
/// of the four-class `nsisWordLists[]` registration at
/// `vendor/lexilla/lexers/LexNsis.cxx:658-663`. Drives
/// `SCE_NSIS_FUNCTION` per the dispatch at `LexNsis.cxx:233-234`.
///
/// **Four-class wordlist surface, this is class 0.** `nsisWordLists[]`
/// declares `"Functions"` / `"Variables"` / `"Lables"` (sic) /
/// `"UserDefined"`, terminated by `nullptr`. Code++ populates
/// classes 0 and 1 only — see [`NSIS_VARIABLES`]. Classes 2
/// (`Lables` — note upstream typo, do NOT silently correct to
/// `"Labels"`) and 3 (`UserDefined`) ship empty in N++'s
/// `langs.model.xml` and Code++ matches.
///
/// **Misleading slot name.** Despite the upstream name `"Functions"`,
/// this is semantically the NSIS **instruction set** — every
/// built-in command (`File`, `SetOutPath`, `MessageBox`,
/// `WriteRegStr`, `CreateDirectory`, `IfFileExists`, etc.)
/// plus every `!`-prefixed compile-time directive NOT in the
/// hard-wired short-circuit set (`!define`, `!include`,
/// `!insertmacro`, `!undef`, `!system`, `!warning`, `!error`,
/// `!verbose`, `!pragma`, etc.). Class 0 is the bulk of the
/// vocabulary — ~200 entries.
///
/// **Case-sensitivity is property-driven, host runs at lexer
/// default.** `LexNsis.cxx:178` reads the `nsis.ignorecase`
/// runtime property; default `0` means strict byte-exact
/// `strcmp` against the source token, value `1` causes the
/// buffered token to be lowercased before `InList` at
/// `:198-202`. Code++ does NOT install the property today
/// (`LangTheme` has no `properties` slot — a follow-up adds it
/// per `docs/lexers-coverage.md`), so the lexer runs at its
/// default `nsis.ignorecase=0`. The wordlist contents below
/// MUST therefore be in **canonical mixed-case** as written in
/// the NSIS Users Manual (`MessageBox`, `SetOutPath`,
/// `WriteRegStr`, …) — this matches the byte-exact source
/// spelling produced by an NSIS script author and matches the
/// hard-wired branches at `:206-231` which also compare against
/// the mixed-case canonical form. (Notepad++'s `langs.model.xml`
/// ships these in lowercase paired with a `nsis.ignorecase=1`
/// property install; once Code++ adds the properties slot,
/// either spelling will work — but the canonical mixed-case
/// form documents author intent independent of the property and
/// will keep working if the property is ever toggled.)
///
/// **Do NOT duplicate hard-wired tokens here.** `classifyWordNsis`
/// at `LexNsis.cxx:206-231` short-circuits on `!macro` /
/// `!macroend` / `!ifdef` / `!ifndef` / `!endif` / `!if` /
/// `!else` / `!ifmacrodef` / `!ifmacrondef` / `Section` /
/// `SectionEnd` / `SubSection` / `SubSectionEnd` /
/// `SectionGroup` / `SectionGroupEnd` / `PageEx` / `PageExEnd` /
/// `Function` / `FunctionEnd` BEFORE consulting any user
/// wordlist — they route to their dedicated `SCE_NSIS_*DEF` /
/// `SECTIONGROUP` / `PAGEEX` / `MACRODEF` / `IFDEFINEDEF` states
/// instead. Adding them here would be unreachable spec noise.
///
/// **No `::` plugin-call recognition.** NSIS source commonly
/// writes plugin invocations as `nsExec::Exec` / `StrFunc::*` /
/// `InstallOptions::*`, but `isNsisChar` at `LexNsis.cxx:63-66`
/// excludes `:`, so the `::` breaks the identifier into two
/// halves that classify independently. For plugin calls to
/// highlight, the wordlist below contains the bare names
/// (`nsExec`, `Exec`, `StrFunc`, `InstallOptions`) rather than
/// the qualified form.
///
/// **Sourcing.** The NSIS Users Manual (Appendix B "Instructions"
/// and Appendix C "Preprocessor") is the canonical source for
/// every entry below. Cross-referenced against N++'s shipped
/// `langs.model.xml` `<Language name="nsis">` `instre1` list
/// for default-set parity. The N++ file is referenced for parity
/// inspection only — no content copied from it (per the
/// CLAUDE.md "no code from Notepad++" rule); the canonical
/// source for every entry below is the NSIS Users Manual.
pub const NSIS_FUNCTIONS: &str = concat!(
    // `!`-directives NOT in the hard-wired short-circuit set
    // (those are `!macro`/`!macroend`/`!if`/`!ifdef`/`!ifndef`/
    // `!else`/`!endif`/`!ifmacrodef`/`!ifmacrondef`). NSIS
    // `!`-directives are canonically lowercase per Users Manual
    // Appendix C — so these match the source spelling under
    // `nsis.ignorecase=0` byte-exact comparison.
    "!addincludedir !addplugindir !appendfile !cd !define !delfile ",
    "!echo !error !execute !finalize !getdllversion !include ",
    "!insertmacro !packhdr !pragma !searchparse !searchreplace ",
    "!system !tempfile !undef !verbose !warning ",
    // Flow-control / call / jump instructions
    "Abort Call CallInstDLL ClearErrors DetailPrint Exec ",
    "ExecShell ExecWait Goto IfErrors IfFileExists IfRebootFlag ",
    "IfSilent IfAbort IntCmp IntCmpU IntFmt IntOp IsWindow ",
    "MessageBox Nop Pop Push Quit Return Sleep ",
    // String / number manipulation
    "StrCmp StrCmpS StrCpy StrLen ",
    // File / directory / path instructions
    "CopyFiles CreateDirectory CreateShortCut Delete ",
    "ExpandEnvStrings File FindClose FindFirst FindNext ",
    "GetFileTime GetFileTimeLocal GetFullPathName GetTempFileName ",
    "Rename RMDir SearchPath SetFileAttributes SetOutPath ",
    // I/O on files (read/write/seek)
    "FileBufSize FileClose FileErrorText FileOpen FileRead ",
    "FileReadByte FileReadUTF16LE FileSeek ",
    "FileWrite FileWriteByte FileWriteUTF16LE ",
    "FlushINI ",
    // Registry / INI
    "DeleteINISec DeleteINIStr DeleteRegKey DeleteRegValue ",
    "EnumRegKey EnumRegValue ReadEnvStr ReadINIStr ReadRegDWORD ",
    "ReadRegStr WriteINIStr WriteRegBin WriteRegDWORD ",
    "WriteRegExpandStr WriteRegStr WriteRegMultiStr ",
    // Section / instType / page metadata setters
    "AddBrandingImage AllowRootDirInstall AllowSkipFiles ",
    "AutoCloseWindow BGFont BGGradient BrandingText BringToFront ",
    "Caption ChangeUI CheckBitmap CompletedText ComponentText ",
    "CRCCheck DirText DirVar DirVerify EnableWindow ",
    "GetCurInstType GetDlgItem GetDLLVersion ",
    "GetDLLVersionLocal GetErrorLevel GetFunctionAddress ",
    "GetInstDirError GetLabelAddress HideWindow Icon ",
    "InstallButtonText InstallColors InstallDir InstallDirRegKey ",
    "InstProgressFlags InstType InstTypeGetText InstTypeSetText ",
    "LockWindow LogSet LogText ",
    // `PageEx` deliberately omitted — it's hard-wired at
    // `LexNsis.cxx:227-228` to `SCE_NSIS_PAGEEX`, short-circuits
    // before the wordlist is consulted, so an entry would be
    // unreachable spec noise (caught by the theme test's
    // hard-wired-shadow guard).
    "MiscButtonText Name OutFile Page PageCallbacks ",
    "Reboot ReserveFile SectionGetFlags SectionGetInstTypes ",
    "SectionGetSize SectionGetText SectionIn SectionSetFlags ",
    "SectionSetInstTypes SectionSetSize SectionSetText ",
    "SendMessage SetAutoClose SetBrandingImage SetCompress ",
    "SetCompressor SetCompressorDictSize SetCtlColors ",
    "SetCurInstType SetDatablockOptimize SetDateSave ",
    "SetDetailsPrint SetDetailsView SetErrorLevel SetErrors ",
    "SetFont SetOverwrite SetPluginUnload SetRebootFlag ",
    "SetRegView SetShellVarContext SetSilent ShowInstDetails ",
    "ShowUninstDetails ShowWindow SilentInstall SilentUnInstall ",
    "SpaceTexts SubCaption UninstallButtonText UninstallCaption ",
    "UninstallExeName UninstallIcon UninstallSubCaption ",
    "UninstallText UninstPage Var WindowIcon XPStyle ",
    // DLL load / unload (NSIS-side)
    "RegDLL UnRegDLL ",
    // Strings / language tables
    "LangString LicenseBkColor LicenseData ",
    "LicenseForceSelection LicenseLangString LicenseText ",
    "LoadLanguageFile ",
    // Plugin invocation bare names (the `::` is not lexed —
    // `nsExec::Exec` splits into `nsExec` and `Exec` halves;
    // these are the namespace halves of the default plugin set).
    "nsExec InstallOptions StrFunc System WinMessages ",
    "UnsafeFile Dialogs nsDialogs Banner AdvSplash Splash ",
    "UserInfo Math LangDLL StartMenu ",
    // Version-info instructions
    "VIAddVersionKey VIProductVersion Unicode InitPluginsDir ",
    // Uninstaller writer + miscellaneous compile-time
    "WriteUninstaller ",
);

/// Space-separated NSIS predefined-variable / numbered-register
/// vocabulary installed via `LexNsis`'s `SCI_SETKEYWORDS(1, ...)`
/// — class 1 of the four-class `nsisWordLists[]` registration at
/// `vendor/lexilla/lexers/LexNsis.cxx:658-663`. Drives
/// `SCE_NSIS_VARIABLE` per the dispatch at `LexNsis.cxx:236-237`.
///
/// **Sigil-included canonical form.** NSIS variables are written
/// in source with a leading `$`, e.g. `$INSTDIR`. The lexer's
/// `classifyWordNsis` at `LexNsis.cxx:252-265` walks the
/// `isNsisChar` characters from the `$` and constructs a buffer
/// that includes the `$` prefix — `s[0] == '$'` is the
/// discriminator at `:252`. So the wordlist entries below MUST
/// include the `$` sigil to match. (The `${...}` brace form is
/// handled separately by a shape check at `:245-248` that does
/// not consult any wordlist — those interpolations always style
/// as `SCE_NSIS_VARIABLE` regardless of class 1 contents.)
///
/// **`nsis.uservars` opt-in extension — not installed today.**
/// When the runtime property `nsis.uservars=1` is set,
/// `classifyWordNsis` at `LexNsis.cxx:252-266` treats ANY
/// `$`-prefixed token of valid `isNsisChar` characters as a
/// variable, bypassing this wordlist. Code++ does NOT install
/// the property today (same `LangTheme`-has-no-properties-slot
/// constraint as `nsis.ignorecase`), so user-declared variables
/// (`Var MyVar` → `$MyVar`) currently lex as `SCE_NSIS_DEFAULT`.
/// Only the predefined names enumerated below highlight. The
/// follow-up that adds the properties slot also installs
/// `nsis.uservars=1` for parity with the N++ default.
///
/// **Case-sensitivity is property-driven, host runs at lexer
/// default** — same contract as `NSIS_FUNCTIONS`. With
/// `nsis.ignorecase=0` (lexer default; Code++ matches by not
/// installing the property), `InList` is byte-exact against the
/// canonical mixed-case source spelling. NSIS predefined
/// variables are written in source ALL-UPPERCASE after the `$`
/// sigil per Users Manual §4.2.3 (`$INSTDIR`, `$WINDIR`, …), so
/// the entries below match that canonical form. The numbered
/// registers `$R0..$R9` use uppercase `R` per the same Users
/// Manual section.
///
/// **Numbered registers (`$0..$9`, `$R0..$R9`)** are NSIS's
/// general-purpose register set, manipulated by `IntOp` /
/// `StrCpy` etc. Included alongside the predefined-folder
/// constants because both share the `SCE_NSIS_VARIABLE` slot
/// semantically — they're "variables provided by the runtime
/// without being declared".
///
/// **Sourcing.** The NSIS Users Manual §4.2 ("Variables") and
/// §4.2.3 ("Constants") is the canonical source for every entry
/// below. Cross-referenced against N++'s `langs.model.xml`
/// `<Language name="nsis">` `instre2` list for default-set
/// parity; no content copied (CLAUDE.md "no code from N++"
/// rule).
pub const NSIS_VARIABLES: &str = concat!(
    // Numbered general-purpose registers (`IntOp $0 ...`, etc.)
    "$0 $1 $2 $3 $4 $5 $6 $7 $8 $9 ",
    "$R0 $R1 $R2 $R3 $R4 $R5 $R6 $R7 $R8 $R9 ",
    // Install / output / system-folder constants
    "$INSTDIR $OUTDIR $CMDLINE $LANGUAGE ",
    "$PROGRAMFILES $PROGRAMFILES32 $PROGRAMFILES64 ",
    "$COMMONFILES $COMMONFILES32 $COMMONFILES64 ",
    "$DESKTOP $EXEDIR $EXEFILE $EXEPATH ",
    "$WINDIR $SYSDIR $TEMP $PLUGINSDIR ",
    // Start menu / shortcut folders
    "$STARTMENU $SMPROGRAMS $SMSTARTUP $QUICKLAUNCH ",
    // Shell-folder constants per SHGetFolderPath
    "$DOCUMENTS $SENDTO $RECENT $FAVORITES ",
    "$MUSIC $PICTURES $VIDEOS $NETHOOD $FONTS ",
    "$TEMPLATES $APPDATA $LOCALAPPDATA $PRINTHOOD ",
    "$INTERNET_CACHE $COOKIES $HISTORY $PROFILE ",
    "$ADMINTOOLS $RESOURCES $RESOURCES_LOCALIZED ",
    "$CDBURN_AREA ",
    // Handles / window state constants
    "$HWNDPARENT ",
);

/// Space-separated TCL built-in command vocabulary installed
/// via `LexTCL`'s `SCI_SETKEYWORDS(0, ...)` — class 0 of the
/// nine-class `tclWordListDesc[]` registration at
/// `vendor/lexilla/lexers/LexTCL.cxx:361-372` (terminated by `0`
/// after `"user4"`). Drives `SCE_TCL_WORD` per the dispatch at
/// `LexTCL.cxx:160-161`.
///
/// **Nine-class wordlist surface, this is class 0.**
/// `tclWordListDesc[]` declares `"TCL Keywords"` / `"TK Keywords"` /
/// `"iTCL Keywords"` / `"tkCommands"` / `"expand"` / `"user1"` /
/// `"user2"` / `"user3"` / `"user4"`. Code++ populates classes
/// 0-3 only — see [`TCL_TK_KEYWORDS`], [`TCL_ITCL_KEYWORDS`], and
/// [`TCL_TK_COMMANDS`]. Classes 4 (`expand` — brace-context-only
/// special class), 5-8 (`user1`..`user4` — user customisation)
/// ship empty in N++'s `langs.model.xml` default and Code++
/// matches.
///
/// **Asymmetric class precedence.** The lexer's match chain at
/// `LexTCL.cxx:160-180` checks classes 0-4 in an `if / else if`
/// first-match-wins chain, then classes 5-8 in a SEPARATE
/// `if / else if` chain that runs UNCONDITIONALLY after — a
/// class-5..8 hit OVERRIDES any class-0..3 classification. Code++
/// keeps classes 5-8 empty to avoid this footgun. Authors adding
/// user1..user4 wordlists should understand the override semantics
/// before populating them.
///
/// **Case-sensitive byte-exact match.** `LexTCL.cxx` has NO case
/// folding — `keywords.InList(s)` at `:160` runs byte-exact against
/// the source token (verified: no `MakeLowerCase` / `tolower` /
/// `GetCurrentLowered` / `CompareCaseInsensitive` anywhere on the
/// wordlist-match path). TCL the language is case-sensitive at the
/// interpreter level — `set` and `SET` are distinct commands — so
/// the lexer's byte-exact posture matches TCL semantics. Wordlist
/// entries below are in their **canonical lowercase** form per the
/// Tcl 8.6 / 9.0 Reference Manual (every built-in command is
/// documented and spelled lowercase: `puts`, `set`, `if`, `proc`,
/// `expr`, `foreach`, etc.). Same byte-exact contract as
/// `LUA_KEYWORDS` / `PERL_KEYWORDS`.
///
/// **Namespace-stripped match.** The lexer strips leading `:`
/// from the candidate buffer at `LexTCL.cxx:156-157` before
/// `InList` — so `::set` source matches the bare `set` wordlist
/// entry. `IsAWordChar` at `:32-35` accepts `:` (the namespace
/// separator), so a fully-qualified `namespace::cmd` traverses as
/// a SINGLE identifier token. To highlight namespaced commands
/// like `string::length` requires the full `namespace::cmd` form
/// in the wordlist (contrast with NSIS's `:`-exclusion which
/// breaks `nsExec::Exec` into two halves).
///
/// **No cross-class duplicates.** A token listed in BOTH class 0
/// (here) AND class 1 (`TCL_TK_KEYWORDS`) hits class 0 first per
/// the `if / else if` chain at `:160-167` — the class-1 entry
/// would be unreachable. The four populated wordlists below maintain
/// disjoint membership, structurally guarded by the
/// `tcl_uses_lextcl_nine_class_theme` test's `HashSet` no-overlap
/// pin.
///
/// **Sourcing.** The Tcl 8.6 / 9.0 Reference Manual ("Built-In
/// Commands" — <https://www.tcl-lang.org/man/tcl/contents.htm>) is
/// the canonical source for the strict interpreter built-ins.
/// Supplemented by commonly-used standard-library procedures
/// from `auto.tcl` / `word.tcl` / `package.tcl` (the `auto_*`
/// family, `tclLog`, `tcl_endOfWord` / `tcl_findLibrary` /
/// `tcl_startOf*Word` / `tcl_wordBreak*`, `pkg_mkIndex`) — these
/// aren't strict built-ins but appear at the top-level shell
/// pervasively enough that N++'s `langs.model.xml` ships them in
/// the same class, and Code++ matches for default-set parity.
/// The N++ file is referenced for parity inspection only, no
/// content copied (CLAUDE.md "no code from Notepad++" rule).
pub const TCL_KEYWORDS: &str = concat!(
    // Variable / scope / namespace commands
    "append array global incr lappend lassign lindex linsert ",
    "list llength lrange lremove lrepeat lreplace lreverse ",
    "lsearch lset lsort namespace set unset upvar uplevel ",
    "variable ",
    // Control flow
    "after break catch continue error eval exit expr for foreach ",
    "if return switch throw try update vwait while ",
    // Procedure / closure
    "apply coroutine proc rename tailcall yield yieldto ",
    // String / regex / format
    "format regexp regsub scan string subst ",
    // I/O / file / channel
    "close eof fblocked fconfigure fcopy fileevent flush gets ",
    "open puts read seek socket source tell ",
    // File system
    "cd file glob pwd ",
    // Process / system
    "auto_execok auto_import auto_load auto_load_index ",
    "auto_qualify auto_reset bgerror clock encoding env exec ",
    "history info interp memory msgcat package pid platform ",
    "pkg_mkIndex registry tcl_endOfWord tcl_findLibrary ",
    "tcl_startOfNextWord tcl_startOfPreviousWord tcl_wordBreakAfter ",
    "tcl_wordBreakBefore tclLog time trace unknown ",
    // Math / binary / conversion
    "binary mathfunc mathop ",
    // Bit / encoding helpers commonly used at the command level
    "concat join split ",
    // Dictionary
    "dict ",
    // Channel-attach / Windows-only DDE / load helpers
    "dde load chan ",
);

/// Space-separated Tk widget-creation command vocabulary
/// installed via `LexTCL`'s `SCI_SETKEYWORDS(1, ...)` — class 1
/// of the nine-class `tclWordListDesc[]` registration at
/// `vendor/lexilla/lexers/LexTCL.cxx:361-372`. Drives
/// `SCE_TCL_WORD2` per the dispatch at `LexTCL.cxx:162-163`.
///
/// **Class 1 = widget-creation commands.** Distinct from class 3
/// (`tkCommands` — see [`TCL_TK_COMMANDS`]) which carries the
/// geometry / event / window-info subcommands. The split mirrors
/// the layered Tk API — class 1 is "construct this widget"
/// (`button`, `label`, `entry`, `frame`, `text`, `canvas`,
/// `toplevel`, …) while class 3 is "manage / query the toolkit"
/// (`pack`, `grid`, `bind`, `winfo`, `wm`, …).
///
/// **Case-sensitive byte-exact match.** Same contract as
/// `TCL_KEYWORDS`. Tk command names are canonically lowercase per
/// the Tcl/Tk Reference Manual.
///
/// **Sourcing.** The Tk Reference Manual ("Built-In Commands" —
/// <https://www.tcl-lang.org/man/tcl/TkCmd/contents.htm>) is the
/// canonical source for every entry below. Cross-referenced
/// against Notepad++'s `langs.model.xml` `instre2` for parity;
/// no content copied (CLAUDE.md "no code from N++" rule).
pub const TCL_TK_KEYWORDS: &str = concat!(
    // Core widget-creation commands
    "button canvas checkbutton entry frame label labelframe ",
    "listbox menu menubutton message panedwindow radiobutton ",
    "scale scrollbar spinbox text toplevel ttk::button ",
    "ttk::checkbutton ttk::combobox ttk::entry ttk::frame ",
    "ttk::label ttk::labelframe ttk::menubutton ttk::notebook ",
    "ttk::panedwindow ttk::progressbar ttk::radiobutton ",
    "ttk::scale ttk::scrollbar ttk::separator ttk::sizegrip ",
    "ttk::spinbox ttk::treeview ",
    // Toolkit-level entry-point commands tied to widget construction
    "tk tkwait ",
    // Tk dialog / utility commands grouped with widget-creation in
    // N++'s shipped class 1
    "tk_bisque tk_chooseColor tk_chooseDirectory tk_dialog ",
    "tk_focusFollowsMouse tk_focusNext tk_focusPrev ",
    "tk_getOpenFile tk_getSaveFile tk_menuSetFocus tk_messageBox ",
    "tk_optionMenu tk_popup tk_setPalette tk_textCopy ",
    "tk_textCut tk_textPaste tkerror ",
);

/// Space-separated `[incr Tcl]` / `TclOO` extension vocabulary
/// installed via `LexTCL`'s `SCI_SETKEYWORDS(2, ...)` — class 2
/// of the nine-class `tclWordListDesc[]` registration at
/// `vendor/lexilla/lexers/LexTCL.cxx:361-372`. Drives
/// `SCE_TCL_WORD3` per the dispatch at `LexTCL.cxx:164-165`.
///
/// **Class 2 = OO extension keywords.** Covers both `[incr Tcl]`
/// (the original Tcl class system) and `TclOO` (the 8.6+ built-in
/// object system) command surfaces. The two systems share a
/// substantial vocabulary (`class`, `method`, `constructor`,
/// `destructor`, `public`, `private`, `protected`) so populating
/// a single wordlist for both matches N++'s default-set posture.
///
/// **Case-sensitive byte-exact match.** Same contract as
/// `TCL_KEYWORDS`. All `[incr Tcl]` and `TclOO` keywords are
/// canonically lowercase per the `itcl(n)` and `TclOO(n)` man
/// pages.
///
/// **All `TclOO` entry points belong here, not in class 0.** The
/// namespace-prefixed `oo::class` / `oo::define` / `oo::object`
/// commands, the call-site keywords `self` / `next` / `my`, and
/// the body keywords (`method`, `constructor`, `destructor`,
/// `superclass`, `mixin`, …) all live in class 2. Maintains the
/// disjoint-membership invariant across [`TCL_KEYWORDS`],
/// [`TCL_TK_KEYWORDS`], [`TCL_ITCL_KEYWORDS`], and
/// [`TCL_TK_COMMANDS`] — structurally pinned by the
/// `tcl_uses_lextcl_nine_class_theme` test's `HashSet` no-overlap
/// guard.
///
/// **Sourcing.** The `[incr Tcl]` Reference Manual (`itcl(n)`,
/// `itclclass(n)`, `itclvars(n)`) and the `TclOO` Reference Manual
/// (`TclOO(n)`, `oo::class(n)`, `oo::define(n)`) are the canonical
/// sources. Cross-referenced against N++'s `langs.model.xml` for
/// parity (N++ ships this class empty by default, so the entries
/// below are Code++'s editorial choice of useful baseline — see
/// `docs/lexers-coverage.md` for the per-language rationale).
pub const TCL_ITCL_KEYWORDS: &str = concat!(
    // `[incr Tcl]` class-body and namespace keywords
    "class inherit ",
    // Access modifiers
    "public private protected ",
    // Member-declaration keywords (used inside class bodies)
    "method constructor destructor common ",
    // TclOO entry-point commands
    "oo::class oo::define oo::object ",
    // TclOO call-site keywords
    "self next my ",
    // TclOO class-definition keywords (used inside `oo::define`)
    "superclass mixin filter export unexport forward ",
    "renamemethod deletemethod ",
    // Object-introspection helpers
    "isa ",
    // `[incr Tcl]` body / configuration helpers
    "body configbody ",
);

/// Space-separated Tk subcommand / geometry-manager / introspection
/// command vocabulary installed via `LexTCL`'s
/// `SCI_SETKEYWORDS(3, ...)` — class 3 of the nine-class
/// `tclWordListDesc[]` registration at
/// `vendor/lexilla/lexers/LexTCL.cxx:361-372`. Drives
/// `SCE_TCL_WORD4` per the dispatch at `LexTCL.cxx:166-167`.
///
/// **Class 3 = Tk management / query commands.** Distinct from
/// class 1 (`TK Keywords` — see [`TCL_TK_KEYWORDS`]) which carries
/// widget-CREATION commands. The lexer's separate-class split
/// follows N++'s shipped `tkCommands` semantic — class 3 is the
/// "manage / query / event" surface (`pack`, `grid`, `place`,
/// `bind`, `bindtags`, `winfo`, `wm`, `event`, …) while class 1
/// is "construct this widget".
///
/// **Case-sensitive byte-exact match.** Same contract as
/// `TCL_KEYWORDS`. All Tk manager / query commands are canonically
/// lowercase per the Tk Reference Manual.
///
/// **No overlap with class 1.** The widget-creation set in
/// [`TCL_TK_KEYWORDS`] (`button`, `canvas`, `entry`, …) is
/// disjoint from this list — verified structurally by the
/// `tcl_uses_lextcl_nine_class_theme` test's `HashSet` no-overlap
/// pin.
///
/// **Sourcing.** The Tk Reference Manual (`pack(n)`, `grid(n)`,
/// `place(n)`, `bind(n)`, `winfo(n)`, `wm(n)`, `event(n)`,
/// `option(n)`, `selection(n)`, `clipboard(n)`, `font(n)`,
/// `tk(n)`, `image(n)`, `focus(n)`, `grab(n)`, `bell(n)`) is the
/// canonical source. Cross-referenced against N++'s
/// `langs.model.xml` `instre3` / `instre4` for parity; no content
/// copied (CLAUDE.md "no code from N++" rule).
pub const TCL_TK_COMMANDS: &str = concat!(
    // Geometry managers
    "pack grid place ",
    // Event / binding management
    "bind bindtags event ",
    // Window / window-manager introspection
    "winfo wm ",
    // Focus / grab / pointer
    "focus grab ",
    // Image / option / clipboard / selection
    "image option clipboard selection ",
    // Sound / display
    "bell ",
    // Window-order / mapping
    "destroy lower raise ",
    // Send / cross-application
    "send ",
    // Font management
    "font ",
);

// PostScript wordlists — installed by `LexPS` via
// `SCI_SETKEYWORDS(class_index, ...)`. The lexer's
// `psWordListDesc[]` at `vendor/lexilla/lexers/LexPS.cxx:327-334`
// declares five classes (0..=4); the level-tier classes (0..=2)
// are populated here, RIP (3) and user-defined (4) are parked
// empty — see the `PS_THEME` install banner in
// `crates/ui_win32/src/lib.rs` for the rationale (both are
// downstream extension points; the LexPS classifier at
// `:156-159` queries them via `InList` on the default-
// constructed empty WordList when the host skips
// `SCI_SETKEYWORDS(3, ...)` / `SCI_SETKEYWORDS(4, ...)`,
// which returns `false` and is safe).
//
// **Case-sensitive byte-exact match.** LexPS calls
// `sc.GetCurrent(s, sizeof(s))` at `LexPS.cxx:155` — NOT
// `GetCurrentLowered` — so wordlist matching is
// **case-sensitive**. PostScript is a case-sensitive language;
// canonical mixed-case identifiers like `FontDirectory`,
// `StandardEncoding`, `ISOLatin1Encoding`, `HalftoneType`, and
// filter names (`ASCII85Decode`, `DCTDecode`, `FlateDecode`,
// …) MUST appear with their canonical case or they will not
// match at scan time.

/// Space-separated PostScript **Level 1** operator vocabulary
/// installed via `LexPS`'s `SCI_SETKEYWORDS(0, ...)` — class 0
/// of the five-class `psWordListDesc[]` at
/// `vendor/lexilla/lexers/LexPS.cxx:327-334`. Gated on
/// `ps.level >= 1` at `:156`; a lower `ps.level` property
/// disables this class. Default `ps.level = 3` (per
/// `:84`) enables all three level tiers.
///
/// **Source:** the PostScript Language Reference, 3rd
/// edition (Adobe, 1999) — Appendix B "Operator Summary"
/// — Level 1 subset. The operator *names* are the public
/// language ABI; no PostScript source or documentation
/// prose is copied. Cross-referenced against Ghostscript's
/// `Resource/Init/gs_lev2.ps` for scope-boundary parity
/// (what belongs in Level 1 vs Level 2).
///
/// **Scope.** The stack / math / array / dictionary / string
/// / boolean / control / type / file / graphics-state / CTM
/// / path / painting / font / VM core available in every
/// PostScript interpreter since Level 1 (1985). Level 2 /
/// Level 3 additions (color, patterns, resources, `DeviceN`,
/// shading, filters, …) live in [`PS_LEVEL2_KEYWORDS`] and
/// [`PS_LEVEL3_KEYWORDS`].
///
/// **Case.** Almost every Level 1 operator is lowercase; the
/// two exceptions carried in this list are `FontDirectory`
/// (the built-in font dictionary) and `StandardEncoding` (the
/// default character encoding). Both canonical mixed-case per
/// PLR §5.3.
pub const PS_LEVEL1_KEYWORDS: &str = concat!(
    // Stack manipulation
    "dup exch pop copy roll index mark cleartomark counttomark clear count ",
    // Math / arithmetic
    "abs add sub mul div idiv mod neg ceiling floor round truncate sqrt ",
    "atan cos sin exp ln log rand srand rrand ",
    // Array (delimiters `[` / `]` are handled by the
    // classifier's PAREN_ARRAY state, not by wordlist)
    "array length get put getinterval putinterval astore aload forall ",
    // Dictionary
    "dict maxlength begin end def load store known undef where ",
    "currentdict systemdict userdict cleardictstack countdictstack dictstack ",
    // String (`length` / `get` / `put` shared with array; not
    // re-listed)
    "string anchorsearch search token ",
    // Boolean / relational / bitwise
    "eq ne gt ge lt le and or not xor bitshift true false ",
    // Control
    "exec if ifelse for repeat loop exit stop stopped ",
    "countexecstack execstack quit start ",
    // Type / conversion / attribute
    "type cvlit cvx executeonly noaccess readonly ",
    "rcheck wcheck xcheck cvi cvr cvn cvs cvrs ",
    // File / stream
    "file closefile read write readhexstring writehexstring ",
    "readstring writestring readline bytesavailable flushfile ",
    "resetfile status run currentfile print echo prompt ",
    // Virtual memory
    "save restore vmstatus ",
    // Graphics state
    "gsave grestore grestoreall initgraphics ",
    "setlinewidth setlinecap setlinejoin setmiterlimit setdash ",
    "setgray sethsbcolor setrgbcolor ",
    "currentlinewidth currentlinecap currentlinejoin currentmiterlimit currentdash ",
    "currentgray currenthsbcolor currentrgbcolor ",
    "setflat currentflat settransfer currenttransfer setscreen currentscreen ",
    // CTM
    "matrix initmatrix identmatrix defaultmatrix currentmatrix setmatrix ",
    "translate scale rotate concat concatmatrix ",
    "transform dtransform itransform idtransform invertmatrix ",
    // Path construction
    "newpath moveto rmoveto lineto rlineto arc arcn arct arcto ",
    "curveto rcurveto closepath flattenpath reversepath strokepath clippath ",
    "currentpoint pathbbox pathforall initclip clip eoclip ",
    // Painting
    "erasepage fill eofill stroke image imagemask ",
    "show ashow widthshow awidthshow kshow stringwidth ",
    // Font
    "findfont scalefont setfont currentfont makefont ",
    "definefont undefinefont FontDirectory StandardEncoding ",
    // Output
    "showpage copypage ",
    // Errors / misc
    "bind null usertime realtime nulldevice ",
);

/// Space-separated PostScript **Level 2** operator vocabulary
/// installed via `LexPS`'s `SCI_SETKEYWORDS(1, ...)` — class 1
/// of `psWordListDesc[]`. Gated on `ps.level >= 2` at
/// `LexPS.cxx:157`.
///
/// **Source:** the PostScript Language Reference, 3rd
/// edition, Appendix B — Level 2 additions (Adobe, 1990
/// introduction of Level 2). Cross-referenced against
/// Ghostscript's `Resource/Init/gs_lev2.ps` for scope-
/// boundary parity.
///
/// **Scope.** Level 2 additions to the operator set:
/// device-independent colour spaces (setters + the
/// discriminators — `DeviceGray` / `CIEBasedA` / `Indexed` /
/// `Pattern` / `Separation` — that name the colour-space
/// families `setcolorspace` selects), patterns, forms,
/// resources, page-device parameters, the `<<`/`>>` dict
/// shorthand (classifier-handled), object serialisation,
/// per-context graphics-state objects, character positioning
/// variants including `glyphshow`, filename enumeration,
/// user-path operators, local/global-VM management (`setglobal`
/// / `currentglobal`), halftone dictionaries with the
/// `HalftoneType` discriminator, and the Level 2 filter
/// mechanism (`ASCII85Decode` / `DCTDecode` / `LZWDecode`
/// / `RunLengthDecode` / `SubFileDecode` / `NullEncode` and
/// their Encode counterparts).
///
/// **Case.** Colour-space discriminators (`DeviceGray`,
/// `CIEBasedA`, …), the halftone discriminator (`HalftoneType`),
/// filter names (`ASCII85Decode`, `DCTDecode`, …), and
/// `ISOLatin1Encoding` are canonical `PascalCase` / `CamelCase`
/// per PLR §3.13, §4.8, §5.3. Every other Level 2 addition is
/// lowercase.
pub const PS_LEVEL2_KEYWORDS: &str = concat!(
    // Colour spaces — setters + discriminators (the family names
    // `setcolorspace` selects via array-head token)
    "setcmykcolor currentcmykcolor setcolor setcolorspace ",
    "currentcolor currentcolorspace ",
    "setcolorrendering currentcolorrendering ",
    "setoverprint currentoverprint colorimage ",
    "DeviceGray DeviceRGB DeviceCMYK ",
    "CIEBasedA CIEBasedABC CIEBasedDEF CIEBasedDEFG ",
    "Indexed Pattern Separation ",
    // Patterns / forms
    "makepattern setpattern execform ",
    // Resource machinery
    "findresource resourcestatus resourceforall ",
    "defineresource undefineresource ",
    // Device / page
    "setpagedevice currentpagedevice ",
    "setdevparams currentdevparams selectdevice ",
    // Fonts + Level 2 glyph-showing
    "selectfont composefont ISOLatin1Encoding glyphshow ",
    // Object serialisation
    "printobject writeobject setobjectformat currentobjectformat ",
    // Graphics-state objects + local/global VM (Level 2's
    // two-VM model per PLR §3.7.2)
    "gstate setgstate currentgstate globaldict languagelevel ",
    "setglobal currentglobal ",
    // Halftones (Level 2 machinery + the `HalftoneType`
    // dictionary-type discriminator)
    "setcolorscreen currentcolorscreen ",
    "setcolortransfer currentcolortransfer ",
    "sethalftone currenthalftone HalftoneType ",
    // Character positioning variants
    "cshow xshow yshow xyshow filenameforall ",
    // User path
    "uappend ucache ucachestatus upath ufill ueofill ustroke ustrokepath ",
    // Level 2 filter mechanism (PLR §3.13). `FlateDecode` /
    // `FlateEncode` / `ReusableStreamDecode` are Level 3
    // additions and live in `PS_LEVEL3_KEYWORDS`.
    "ASCII85Decode ASCII85Encode ASCIIHexDecode ASCIIHexEncode ",
    "DCTDecode DCTEncode CCITTFaxDecode CCITTFaxEncode ",
    "LZWDecode LZWEncode RunLengthDecode RunLengthEncode ",
    "SubFileDecode NullEncode ",
);

/// Space-separated PostScript **Level 3** operator vocabulary
/// installed via `LexPS`'s `SCI_SETKEYWORDS(2, ...)` — class 2
/// of `psWordListDesc[]`. Gated on `ps.level >= 3` at
/// `LexPS.cxx:158`.
///
/// **Source:** the PostScript Language Reference, 3rd
/// edition (Adobe, 1999) — Level 3 additions (Adobe, 1997
/// introduction of Level 3 alongside PDF 1.2). Cross-
/// referenced against Ghostscript's Level-3 resource files
/// for parity.
///
/// **Scope, minimal by design.** Only the genuine Level 3
/// additions live here — the Level 2 filter mechanism
/// (`ASCII85Decode` / `DCTDecode` / `LZWDecode` / …), the
/// Level 2 colour-space discriminators (`DeviceGray` /
/// `CIEBasedA` / `Indexed` / `Pattern` / `Separation`), the
/// Level 2 `HalftoneType` discriminator, and Level 2's
/// local/global-VM operators (`setglobal` / `currentglobal`)
/// and `glyphshow` are ALL in [`PS_LEVEL2_KEYWORDS`] because
/// that is where the PostScript Language Reference places
/// them (§3.7.2, §3.13, §4.5.6, §4.8, §7.4). Mis-classifying
/// them as Level 3 works accidentally at the default
/// `ps.level = 3` (`LexPS`'s `:156-159` chain always fires
/// class 2's `InList` when the setting is 3), but silently
/// hides those operators when a user's session or `.psrc`
/// sets `ps.level = 1` or `2`.
///
/// The genuine Level 3 additions are:
///   - Smooth shading (`shfill` / `setsmoothness` /
///     `currentsmoothness`).
///   - Idiom recognition (`setidiomrecognition` /
///     `currentidiomrecognition`).
///   - The `DeviceN` colour space (the one colour-space
///     family Level 3 added on top of Level 2's set).
///   - Flate compression (`FlateDecode` / `FlateEncode`) and
///     the `ReusableStreamDecode` filter (added alongside
///     the reusable-stream and PDF-1.2 image models).
///
/// **Case.** `FlateDecode` / `FlateEncode` /
/// `ReusableStreamDecode` are canonical `CamelCase` per PLR
/// §3.13; `DeviceN` is canonical `PascalCase` per PLR §4.8.
pub const PS_LEVEL3_KEYWORDS: &str = concat!(
    // Smooth shading
    "shfill setsmoothness currentsmoothness ",
    // Idiom recognition
    "setidiomrecognition currentidiomrecognition ",
    // The one Level 3 colour-space addition
    "DeviceN ",
    // Level 3 stream-filter additions
    "FlateDecode FlateEncode ReusableStreamDecode ",
);

/// Space-separated Ruby reserved-word vocabulary installed via
/// `LexRuby`'s `SCI_SETKEYWORDS(0, ...)` — the sole class of
/// `rubyWordListDesc[]` at
/// `vendor/lexilla/lexers/LexRuby.cxx:142-145`. Drives
/// `SCE_RB_WORD` (and, when a keyword is used as a trailing
/// statement modifier and matches `keywordIsAmbiguous` at
/// `:1793-1797`, `SCE_RB_WORD_DEMOTED`) via the classifier at
/// `:358-374`.
///
/// **Case-sensitive byte-exact match.** `ClassifyWordRb` at
/// `:335-337` calls `styler.GetRange(start, end)` — no
/// `GetCurrentLowered` wrapper — so `BEGIN` / `END` / `__FILE__`
/// / `__LINE__` / `__ENCODING__` are canonical uppercase /
/// double-underscore-magic entries and MUST appear with their
/// exact case. `defined?` is admitted at the token-boundary
/// level by `:1418-1425`'s special path that extends an
/// identifier segment across a trailing `?` / `!` — the
/// wordlist entry `defined?` (with the `?`) matches the
/// segment `styler.GetRange` produces.
///
/// **Source.** The Ruby Language Reference (ISO/IEC 30170:2012
/// §11 "Keywords" + community MRI documentation of the Ruby
/// 3.x reserved-word set). The keyword *names* are the public
/// language ABI; no Ruby source or documentation prose is
/// copied. Cross-referenced against N++'s shipped
/// `langs.model.xml` `<Language name="ruby">` `instre1` for
/// default-set parity; no content copied (CLAUDE.md "no code
/// from N++" rule).
///
/// **Scope.** Strict reserved-word set (41 entries — the
/// Ruby 3.x reserved-word list per `docs.ruby-lang.org`'s
/// keyword page). Excludes Kernel methods entirely — `puts`,
/// `print`, `warn`, `eval` (which `LexRuby` handles via its
/// own special-case at `:393-395` that promotes them to the
/// pseudo-style `SCE_RB_IDENTIFIER_PREFERRE` regardless of
/// wordlist membership); AND every other `Kernel` method
/// like `raise`, `throw`, `catch`, `loop`, `lambda`, `proc`,
/// `require`, `require_relative`, `load`, `attr_accessor` /
/// `attr_reader` / `attr_writer`, `__method__` (the current-
/// method-name reflection helper) — none of these are
/// reserved words, and listing them here would incorrectly
/// paint them bold-keyword when they're just ordinary
/// method calls. Excludes constants (`STDIN`, `STDOUT`,
/// `STDERR`, `ARGV`, `ENV`, `RUBY_VERSION`) — those are
/// host-emitted via their own `SCE_RB_*` slots (`STDIN` =
/// 30, `STDOUT` = 31, `STDERR` = 40, all directly emitted
/// by the classifier state machine) or paint as bare
/// identifiers.
pub const RUBY_KEYWORDS: &str = concat!(
    // Definition keywords
    "class module def end alias undef ",
    // Control flow — leaders (statement-heading)
    "if elsif else unless case when then while until for do ",
    "break next redo retry return yield ",
    // Exception handling. `raise` is intentionally EXCLUDED —
    // it's a Kernel method (`Kernel#raise`), not a reserved word.
    "begin rescue ensure ",
    // Boolean / nil / self / super
    "true false nil self super ",
    // Logical operators (word form)
    "and or not ",
    // Introspection
    "defined? in ",
    // Top-level blocks (canonical uppercase)
    "BEGIN END ",
    // Magic constants (double-underscore, uppercase). `__method__`
    // is intentionally EXCLUDED — it's a Kernel method that
    // returns the current-method name symbol, not a reserved word.
    "__FILE__ __LINE__ __ENCODING__ ",
);

/// Space-separated Smalltalk **special-selector** vocabulary
/// installed via `LexSmalltalk`'s `SCI_SETKEYWORDS(0, ...)`
/// — the sole class of `smalltalkWordListDesc[]` at
/// `vendor/lexilla/lexers/LexSmalltalk.cxx:325-328`. Drives
/// `SCE_ST_SPEC_SEL` via the classifier at `:250-251` — an
/// identifier promoted to `SCE_ST_SPEC_SEL` when it matches
/// this wordlist, otherwise it stays at `SCE_ST_KWSEND`
/// (for `keyword:`-suffixed idents) or falls through the
/// hardcoded strcmp chain at `:257-266` for the 5 language
/// constants (`self` / `super` / `nil` / `true` / `false`).
///
/// **Case-sensitive byte-exact match.** `handleLetter` at
/// `:223-270` builds the identifier via
/// `isAlphaNumeric` chars from `ClassificationTable[]` at
/// `:71-80` (no folding) then dispatches
/// `wordLists[0]->InList(ident)` at `:250`. Wordlist entries
/// must match the source's exact case. Smalltalk is a
/// case-sensitive language.
///
/// **Per-keyword-part, not compound.** `handleLetter` reads
/// alphanumeric chars then admits AT MOST ONE trailing `:`
/// (`:241-247` — `doubleColonPresent` is `bool`, not a
/// counter). So a compound selector like `ifTrue:ifFalse:`
/// is tokenised as TWO separate atoms `ifTrue:` and
/// `ifFalse:`. Entries in this wordlist must be
/// single-keyword-part atoms (`ifTrue:`, `ifFalse:`) —
/// NEVER compound (`ifTrue:ifFalse:`), which would be
/// unreachable.
///
/// **Do NOT list `self` / `super` / `nil` / `true` /
/// `false`.** The `handleLetter` dispatch order at
/// `:250-266` is `InList` (first, line 250) →
/// `doubleColonPresent` (252) → `isUpper(ident[0])` (254)
/// → hardcoded strcmp chain (`:257-266`, as a last-chance
/// fallback for bare lowercase idents). If any of these
/// five were added to this wordlist, `InList` would fire
/// FIRST and silently promote them to `SCE_ST_SPEC_SEL`,
/// OVERRIDING the dedicated `SCE_ST_SELF` / `SUPER` /
/// `NIL` / `BOOL` styles they'd otherwise land in via the
/// hardcoded fallback — the opposite of the intended
/// visual differentiation. They're excluded because
/// `InList` would win a precedence it shouldn't, not
/// because it would lose to something else.
///
/// **Source.** `SciTE`'s default Smalltalk `.properties` file
/// at `vendor/lexilla/test/examples/smalltalk/SciTE.properties:2`
/// ships an 11-selector default (`ifTrue: ifFalse:
/// whileTrue: whileFalse: ifNil: ifNotNil: whileTrue
/// whileFalse repeat isNil notNil`). Code++ extends this with
/// the 4 boolean short-circuit combinators (`and:` / `or:` /
/// `xor:` / `not`) that also read as control-flow constructs
/// at read-time. Total 15 entries. Cross-referenced against
/// the Blue Book (ANSI Smalltalk / Squeak / Pharo control-
/// flow protocols) — no code copied.
///
/// **Scope, deliberately minimal.** This wordlist is for
/// selectors that visually read as language keywords
/// (`ifTrue:` is Smalltalk's `if`; `whileTrue:` is its
/// `while`; `and:` is short-circuit boolean-and). Ordinary
/// method-send selectors like `at:` / `put:` / `do:` /
/// `collect:` / `printString` are NOT in this list — they
/// paint as `SCE_ST_KWSEND` (steel-blue) which is the
/// correct "keyword-send but not a control primitive"
/// styling. Adding them here would over-bold ordinary
/// message sends and defeat the visual signal.
pub const SMALLTALK_SPECIAL_SELECTORS: &str = concat!(
    // Boolean-conditional control flow (single-part atoms;
    // the compound `ifTrue:ifFalse:` is tokenised as two
    // atoms, so both parts must be listed separately)
    "ifTrue: ifFalse: ",
    // Nil-conditional control flow
    "ifNil: ifNotNil: ",
    // Iteration control flow
    "whileTrue: whileFalse: whileTrue whileFalse repeat ",
    // Nil predicates (unary — no trailing `:`)
    "isNil notNil ",
    // Boolean short-circuit combinators
    "and: or: xor: not ",
);

/// Space-separated VHDL **reserved-word** vocabulary installed via
/// `LexVHDL`'s `SCI_SETKEYWORDS(0, ...)` — the first class of
/// `VHDLWordLists[]` at
/// `vendor/lexilla/lexers/LexVHDL.cxx:552-561`. Drives
/// `SCE_VHDL_KEYWORD` via the identifier-exit classifier at
/// `LexVHDL.cxx:93-94`: on scan exit, the wordlist chain probes
/// this list FIRST and promotes matching identifiers from
/// `SCE_VHDL_IDENTIFIER` to `SCE_VHDL_KEYWORD`.
///
/// **Case-insensitive language, byte-exact wordlist.** VHDL is
/// case-insensitive per IEEE-1076 §13.4 — `ENTITY` and `entity`
/// are the same reserved word. The classifier calls
/// `GetCurrentLowered(s, sizeof(s))` at `LexVHDL.cxx:92` before
/// every wordlist probe, so `InList` receives a case-folded
/// (lowercase) identifier. Wordlist entries MUST be lowercase —
/// an uppercase entry would never match. Same convention as
/// `PS_LEVEL1_KEYWORDS` (also case-insensitive).
///
/// **Source.** IEEE-1076-1993 §13.9 reserved-word list, extended
/// to IEEE-1076-2002's `protected`. Cross-referenced against the
/// upstream Scintilla author's documented list at
/// `vendor/lexilla/lexers/LexVHDL.cxx:568-573` — a `//
/// Keyword:` commented enumeration. That list is 81 words
/// (`access` through `with`); `protected` was NOT in Scintilla's
/// -93-vintage enumeration but is a legitimate VHDL-2002+
/// reserved word (used in the classifier's own fold routine's
/// keyword string at `LexVHDL.cxx:238-239` and fold-trigger
/// `strcmp` at `:403`), so we include it. VHDL-2008 additions (`assume`, `context`,
/// `cover`, `default`, `fairness`, `force`, `parameter`,
/// `property`, `release`, `restrict`, `sequence`, `strong`,
/// `vunit`, etc.) are intentionally excluded pending broader
/// VHDL-2008 syntax coverage — the fold routine doesn't fold on
/// them either, so adding them here without matching folder
/// work would create an inconsistency.
///
/// **The `range` overlap.** `range` appears in BOTH this
/// wordlist (as a reserved word — `range 0 to 7` in a subtype
/// declaration) AND `VHDL_ATTRIBUTES` (as an attribute
/// designator — `T'range`). The lexer's dispatch order at
/// `LexVHDL.cxx:93-107` probes classes 0..6 sequentially; class
/// 0 (Keywords) is checked FIRST at `:93`, class 2 (Attributes)
/// at `:97`. So `range` in this list ALWAYS wins over the
/// Attributes-list entry — a `T'range` token paints `range` as
/// `SCE_VHDL_KEYWORD` (the tick itself painting as
/// `SCE_VHDL_OPERATOR`). This precedence matches Scintilla's
/// upstream behaviour and Notepad++'s ship default.
pub const VHDL_KEYWORDS: &str = concat!(
    // Declaration keywords
    "access after alias all architecture array assert attribute ",
    "begin block body buffer bus ",
    "case component configuration constant ",
    "disconnect downto ",
    "else elsif end entity exit ",
    "file for function generate generic group guarded ",
    "if impure in inertial inout is ",
    "label library linkage literal loop ",
    "map new next null ",
    "of on open others out ",
    "package port postponed procedure process protected pure ",
    "range record register reject report return ",
    "select severity shared signal subtype ",
    "then to transport type ",
    "unaffected units until use ",
    "variable wait when while with ",
);

/// Space-separated VHDL **word-form operator** vocabulary
/// installed via `LexVHDL`'s `SCI_SETKEYWORDS(1, ...)` — the
/// second class of `VHDLWordLists[]` at `LexVHDL.cxx:552-561`.
/// Drives `SCE_VHDL_STDOPERATOR` via classifier at
/// `LexVHDL.cxx:95-96` when the identifier fails the KEYWORD
/// probe but matches this list. Case-insensitive per
/// `GetCurrentLowered` at `:92`.
///
/// **Scope.** IEEE-1076 §7.2 defines 16 word-form operators
/// (`abs`, `and`, `mod`, `nand`, `nor`, `not`, `or`, `rem`,
/// `rol`, `ror`, `sla`, `sll`, `sra`, `srl`, `xnor`, `xor`).
/// Distinct from punctuation-class operators (`+ - * / = < > <= >= /=`)
/// which paint as `SCE_VHDL_OPERATOR` via `isoperator` at
/// `:169-170`. The dual style lets the theme colour word
/// operators (which read as identifiers to the eye) distinctly
/// from punctuation ones.
///
/// **Case-insensitive language, byte-exact wordlist.** Same
/// case-folding rule as `VHDL_KEYWORDS` — entries lowercase.
pub const VHDL_OPERATORS: &str = concat!(
    "abs and mod nand nor not or rem ",
    "rol ror sla sll sra srl xnor xor ",
);

/// Space-separated VHDL **predefined-attribute** vocabulary
/// installed via `LexVHDL`'s `SCI_SETKEYWORDS(2, ...)` — the
/// third class of `VHDLWordLists[]` at `LexVHDL.cxx:552-561`.
/// Drives `SCE_VHDL_ATTRIBUTE` via classifier at
/// `LexVHDL.cxx:97-98` when the identifier fails KEYWORD and
/// STDOPERATOR probes but matches this list.
///
/// **Attribute designator, not the tick.** VHDL attributes are
/// accessed via `T'attr` syntax (a tick between the prefix and
/// the attribute designator). The lexer handles the tick via a
/// dedicated `else if (sc.ch == '\'')` branch at `LexVHDL.cxx:155-165`
/// (sibling to the `isoperator` branch at `:169-170`, so the
/// tick can never fall through to `SCE_VHDL_OPERATOR`); in the
/// common attribute-access case (multi-character attribute name),
/// that branch calls no `SetState`, so the tick stays as
/// `SCE_VHDL_DEFAULT`. The designator identifier itself is
/// separately promoted to `SCE_VHDL_ATTRIBUTE`. Wordlist entries
/// are the designator only (no leading tick).
///
/// **Case-insensitive language, byte-exact wordlist.** Same
/// case-folding rule as `VHDL_KEYWORDS`.
///
/// **The `range` overlap** — see `VHDL_KEYWORDS` rationale.
/// `range` appears here for completeness (matches upstream) but
/// its Attributes-list entry is dead code because
/// class 0 (Keywords) fires first at `:93`.
///
/// **Source.** IEEE-1076-1993 §14.1 predefined attributes.
/// Cross-referenced against upstream banner at
/// `LexVHDL.cxx:578-581`.
pub const VHDL_ATTRIBUTES: &str = concat!(
    // Scalar type attributes
    "left right low high ascending image value pos val succ pred ",
    "leftof rightof base range reverse_range ",
    // Array attributes
    "length ",
    // Signal attributes
    "delayed stable quiet transaction event active ",
    "last_event last_active last_value driving driving_value ",
    // Name-string attributes
    "simple_name path_name instance_name ",
);

/// Space-separated VHDL **standard-function** vocabulary
/// installed via `LexVHDL`'s `SCI_SETKEYWORDS(3, ...)` — the
/// fourth class of `VHDLWordLists[]` at `LexVHDL.cxx:552-561`.
/// Drives `SCE_VHDL_STDFUNCTION` via classifier at
/// `LexVHDL.cxx:99-100`.
///
/// **Source.** Functions defined by the IEEE-1076 standard
/// packages (`std.textio`, `ieee.std_logic_1164`,
/// `ieee.numeric_std`, `ieee.numeric_bit`) plus the fixed
/// `std.standard` namespace utilities. Cross-referenced against
/// upstream banner at `LexVHDL.cxx:583-586`.
///
/// **Case-insensitive language, byte-exact wordlist.** Entries
/// are lowercase — the upstream banner's `to_UX01` is written
/// mixed-case to reflect the IEEE-1164 uppercase convention for
/// the target type name, but the lexer lowercases before match
/// (`GetCurrentLowered` at `:92`) so the wordlist MUST use
/// `to_ux01`. Same applies elsewhere in the list.
pub const VHDL_STDFUNCTIONS: &str = concat!(
    // std.textio I/O
    "now readline read writeline write endfile ",
    // std_logic_1164 conversion + resolution
    "resolved to_bit to_bitvector to_stdulogic to_stdlogicvector to_stdulogicvector ",
    "to_x01 to_x01z to_ux01 ",
    // Edge detectors
    "rising_edge falling_edge is_x ",
    // numeric_std / numeric_bit shifts + rotates + resize + coercions
    "shift_left shift_right rotate_left rotate_right resize ",
    "to_integer to_unsigned to_signed std_match to_01 ",
);

/// Space-separated VHDL **standard-package** vocabulary
/// installed via `LexVHDL`'s `SCI_SETKEYWORDS(4, ...)` — the
/// fifth class of `VHDLWordLists[]` at `LexVHDL.cxx:552-561`.
/// Drives `SCE_VHDL_STDPACKAGE` via classifier at
/// `LexVHDL.cxx:101-102`.
///
/// **Source.** IEEE-1076-2008 §16 standard packages plus the
/// three fixed libraries (`std`, `ieee`, `work`) that every
/// VHDL design references. `work` is the implicit current-
/// design library. Cross-referenced against upstream banner at
/// `LexVHDL.cxx:588-591`.
///
/// **Case-insensitive language, byte-exact wordlist.** Same
/// case-folding rule as `VHDL_KEYWORDS`.
pub const VHDL_STDPACKAGES: &str = concat!(
    // Libraries
    "std ieee work ",
    // std library packages
    "standard textio ",
    // ieee library packages (synthesis + arith)
    "std_logic_1164 std_logic_arith std_logic_misc ",
    "std_logic_signed std_logic_textio std_logic_unsigned ",
    "numeric_bit numeric_std ",
    // ieee math packages
    "math_complex math_real ",
    // ieee VITAL packages (timing)
    "vital_primitives vital_timing ",
);

/// Space-separated VHDL **standard-type** vocabulary installed
/// via `LexVHDL`'s `SCI_SETKEYWORDS(5, ...)` — the sixth class
/// of `VHDLWordLists[]` at `LexVHDL.cxx:552-561`. Drives
/// `SCE_VHDL_STDTYPE` via classifier at `LexVHDL.cxx:103-104`.
///
/// **Source.** Predefined types from `std.standard` (`boolean`,
/// `bit`, `integer`, `real`, `time`, `natural`, `positive`,
/// `character`, `string`, `bit_vector`, plus the file-open
/// enumerations), from `std.textio` (`line`, `text`, `side`,
/// `width`), and from `ieee.std_logic_1164` (`std_ulogic`,
/// `std_ulogic_vector`, `std_logic`, `std_logic_vector`, and
/// the four subtype constants `x01`, `x01z`, `ux01`, `ux01z`).
/// `unsigned` / `signed` come from `ieee.numeric_std`.
/// Cross-referenced against upstream banner at
/// `LexVHDL.cxx:593-596`.
///
/// **Case-insensitive language, byte-exact wordlist.** The
/// upstream banner writes `X01` / `X01Z` / `UX01` / `UX01Z` in
/// uppercase to reflect IEEE-1164's uppercase convention for the
/// logic-value type names, but the lexer lowercases before
/// match — wordlist entries MUST be lowercase (`x01`, `x01z`,
/// etc.).
pub const VHDL_STDTYPES: &str = concat!(
    // std.standard scalars
    "boolean bit character severity_level integer real time delay_length ",
    "natural positive ",
    // std.standard arrays
    "string bit_vector ",
    // std.standard file-open enumerations
    "file_open_kind file_open_status ",
    // std.textio
    "line text side width ",
    // ieee.std_logic_1164 (types + subtype constants)
    "std_ulogic std_ulogic_vector std_logic std_logic_vector ",
    "x01 x01z ux01 ux01z ",
    // ieee.numeric_std
    "unsigned signed ",
);

/// Space-separated VHDL **user-word** vocabulary installed via
/// `LexVHDL`'s `SCI_SETKEYWORDS(6, ...)` — the seventh (and
/// last) class of `VHDLWordLists[]` at `LexVHDL.cxx:552-561`.
/// Drives `SCE_VHDL_USERWORD` via classifier at
/// `LexVHDL.cxx:105-106`.
///
/// **Deliberately empty.** This class is the per-project
/// extension slot — the VHDL lexer author designed it as an
/// opt-in surface for project-specific identifiers (module
/// names, custom-package types) that a user's `.properties`
/// override could populate. Code++ ships it empty (a valid
/// `WordList` with zero entries) so the class-index dispatch
/// at `:105-106` still fires without falsely promoting any
/// identifier. When Code++ grows a per-project override
/// surface, this constant becomes the default-empty value the
/// user config layers over.
///
/// **Empty install is required, not skippable.** `LexerBase`
/// pre-allocates `KEYWORDSET_MAX + 1 = 9` `WordList*` slots at
/// construction (`LexerBase.h:19` enum + `LexerBase.cxx:32-34`
/// loop) — well past the 7 that `VHDLWordLists[]` names, so
/// slot 6 exists unconditionally. The classifier at
/// `LexVHDL.cxx:105` addresses slot 6 whether or not it was
/// installed, so an unset slot 6 would still receive
/// `InList(s)` calls against a fresh empty list (safe:
/// returns false). Installing an empty string via
/// `SCI_SETKEYWORDS(6, "")` writes an explicit empty
/// `WordList`, which is the safer guarantee than relying on
/// zero-init behaviour.
pub const VHDL_USERWORDS: &str = "";

/// Space-separated `KIXtart` **command** vocabulary installed via
/// `LexKix`'s `SCI_SETKEYWORDS(0, ...)` — `keywords` (class 0) at
/// `vendor/lexilla/lexers/LexKix.cxx:44`. Drives `SCE_KIX_KEYWORD`
/// via the identifier-exit classifier at `LexKix.cxx:100-101`:
/// on scan exit, `keywords.InList(s)` is probed FIRST (before
/// `keywords2`), and matches are promoted from `SCE_KIX_IDENTIFIER`
/// to `SCE_KIX_KEYWORD`.
///
/// **Scope: commands, not functions.** `KIXtart` splits its
/// vocabulary into two visually-distinct categories: **commands**
/// (statement-heading; drive control flow, filesystem/registry
/// side effects, screen I/O) and **functions** (expression-usable;
/// return values). Only commands belong here. Functions live in
/// `KIX_FUNCTIONS` (class 1). The lexer paints each with a
/// distinct style so a `KIXtart` author can visually verify a token
/// is used in its intended slot — a `use` on the right-hand side
/// of `$x = use()` is almost certainly a bug because `use` is a
/// command, not a function.
///
/// **Case-insensitive language, byte-exact wordlist.** `KIXtart` is
/// case-insensitive: `IF` and `if` are the same command. The
/// classifier calls `GetCurrentLowered(s, sizeof(s))` at
/// `LexKix.cxx:98` before `InList`, so wordlist entries MUST be
/// lowercase — an uppercase entry would never match. Same
/// convention as `VHDL_KEYWORDS` and `PS_LEVEL1_KEYWORDS`.
///
/// **Source.** `KIXtart` 4.x language reference (the last stable
/// release-family before the language went dormant in ~2018).
/// Cross-referenced against the `KIXtart` community's `kix.dtd` /
/// `kix.xml` help schema and the Notepad++ 8.x shipped default
/// `KIXtart` user-defined-language definition. No code copied.
pub const KIX_KEYWORDS: &str = concat!(
    // Control flow
    "if else endif ",
    "while loop until do ",
    "for each next to step in ",
    "select case endselect ",
    "break exit continue ",
    // User-defined functions + procedure control
    "function endfunction ",
    "gosub return goto call ",
    // Variable declarations
    "dim redim global ",
    // Filesystem statement commands
    "use del copy move md rd cd ",
    "run shell ",
    // Console + I/O statement commands
    "sleep beep big small flushkb debug ",
    "cls color at ",
    "get gets password ",
    // System statement commands
    "settime include ",
    // NOTE: `?` / `??` (KIXtart print-newline / print-no-newline) and
    // registry / printer / config command-forms (addkey / delkey /
    // writevalue / delvalue / addprinterconnection / logevent /
    // settitle / setconsole / setl / setm / setascii / setoption /
    // setwallpaper / setfileattr) are INTENTIONALLY ABSENT.
    //
    // `?` / `??` cannot reach the identifier-exit path — `IsAWordChar`
    // at `LexKix.cxx:33-35` excludes `?` (0x3F: not isalnum, not `_`,
    // not >=0x80) and `IsOperator` at `:37-39` excludes it too (the
    // 9-char operator set is `+ - * / & | < > =` only), so the state
    // machine at `:110-129` never transitions to `SCE_KIX_IDENTIFIER`
    // on `?` and `keywords.InList("?")` is never called. Adding the
    // tokens here would be dead code.
    //
    // The registry / printer / config forms are all documented as
    // FUNCTIONS in the `KIXtart` 4.x reference (each returns a
    // status code and is idiomatically used in expression context —
    // `$err = WriteValue(...)`, `If AddKey(...) = 0`). They live in
    // `KIX_FUNCTIONS`. Duplicating them here would silently mask the
    // FUNCTIONS entry because `LexKix.cxx:100-103` probes `keywords`
    // FIRST, defeating the commands-vs-functions visual contract.
);

/// Space-separated `KIXtart` **built-in-function** vocabulary
/// installed via `LexKix`'s `SCI_SETKEYWORDS(1, ...)` — `keywords2`
/// (class 1) at `LexKix.cxx:45`. Drives `SCE_KIX_FUNCTIONS` via
/// the identifier-exit classifier at `LexKix.cxx:102-103`: on scan
/// exit, if `keywords.InList(s)` returned false, `keywords2.InList(s)`
/// is probed — matches promote from `SCE_KIX_IDENTIFIER` to
/// `SCE_KIX_FUNCTIONS`.
///
/// **Scope: expression-usable, return values.** See `KIX_KEYWORDS`
/// for the commands-vs-functions distinction. This list holds the
/// `KIXtart` 4.x built-in function surface — string utilities,
/// filesystem queries, registry queries, numeric conversions,
/// object interop (`CreateObject` / `GetObject` for COM), and system
/// info.
///
/// **Case-insensitive language, byte-exact wordlist.** Same
/// case-folding rule as `KIX_KEYWORDS`. Entries lowercase.
///
/// **Source.** `KIXtart` 4.x language reference. No code copied.
pub const KIX_FUNCTIONS: &str = concat!(
    // Numeric conversion / math
    "abs cdbl cint cstr chr asc dectohex ",
    "iif rnd round srnd val vartype vartypename typecast ",
    "formatnumber ",
    // String utilities
    "left right substr len instr instrrev ",
    "lcase ucase ltrim rtrim trim replace join ",
    "asciitochr ",
    // Array utilities
    "ubound ascan ",
    // Filesystem / files
    "dir fileexists exist existkey ",
    "getfileattr getfilesize getfiletime getfileversion ",
    "comparefiletimes deltree freefilehandle ",
    "open close readline writeline redirectoutput ",
    // Process
    "setdefaultprinter shutdown logoff execute setsystemstate ",
    // Registry
    "readvalue writevalue delvalue ",
    "addkey delkey enumkey enumvalue savedkey ",
    "loadhive unloadhive savekey ",
    "readtype readprofilestring writeprofilestring ",
    // Environment + system state
    "expandenvironmentvars macros memorysize ",
    "getdiskspace inifile addprogramgroup addprogramitem ",
    "delprogramgroup delprogramitem showprogramgroup ",
    "logevent backupeventlog cleareventlog ",
    "addprinterconnection delprinterconnection ",
    "in ingroup isdeclared enumgroup enumlocalgroup enumipinfo ",
    "setfileattr setl setm setascii setconsole setoption ",
    "settitle setwallpaper setfocus ",
    // Object interop (COM)
    "createobject getobject ",
    // Input + UI
    "box messagebox sendkeys sendmessage senddata ",
    // Identity + naming
    "sidtoname ",
);

/// Space-separated `KIXtart` **macro-name** vocabulary installed via
/// `LexKix`'s `SCI_SETKEYWORDS(2, ...)` — `keywords3` (class 2) at
/// `LexKix.cxx:46`. Drives the MACRO whitelist gate at
/// `LexKix.cxx:81-89`: a `@name` token enters `SCE_KIX_MACRO`
/// state at `:121-122` and, on scan exit, the identifier AFTER
/// the `@` (`&s[1]` at `:86`) is probed against this list. If
/// present, MACRO stays. If absent, MACRO DOWNGRADES to DEFAULT
/// at `:87-88`. **This wordlist is a whitelist**, not a
/// dictionary — its whole purpose is to catch typos in macro
/// names.
///
/// **Names WITHOUT the `@` prefix.** The classifier probes
/// `&s[1]` (byte 1 onward — the identifier after the sigil), so
/// wordlist entries are the bare macro name. `@date` sends
/// `date` to `InList`; the wordlist entry MUST be `date`, not
/// `@date`.
///
/// **Case-insensitive language, byte-exact wordlist.** Same
/// case-folding rule as `KIX_KEYWORDS` (via `GetCurrentLowered`
/// at `:84`). Entries lowercase. `@DATE` and `@date` both
/// case-fold to `date` before the whitelist probe.
///
/// **Source.** `KIXtart` 4.x language reference — the full built-in
/// macro surface. `KIXtart` has ~80 macros covering identity
/// (user / computer / domain), time (date / time / ticks),
/// network (IP / hostname / mapped drives), system config
/// (OS version / CPU / memory), and script metadata
/// (script name / dir / result). No user extension — the
/// macro namespace is fixed by the `KIXtart` runtime.
pub const KIX_MACROS: &str = concat!(
    // Identity
    "userid username fullname wksta ",
    "wuserid userlang priv primarygroup ",
    "homedir homedrive homeshare longhomedir ",
    "sid ",
    // Domain / server
    "domain ldomain ldomainid lserver rserver ",
    "site sdomain ",
    // Network
    "address hostname ",
    "ipaddress0 ipaddress1 ipaddress2 ipaddress3 ",
    "connectmode ",
    "ldrive ldriveid ldriveparent ldriveroot ",
    "ldriveservice ldrivetype ",
    // Time / date
    "date day month year time ",
    "mdayno wdayno wday monthno ",
    "ticks msecs ",
    // System info
    "cpu mhz build csd dos inwin kix ",
    "resolution ",
    "prodsuite producttype ",
    "syslang tssession pid ras inwow64 onwow64 ",
    "maxpwage pwage ",
    // Script metadata
    "scriptdir scriptexe scriptname ",
    "startdir curdir cwd ",
    "result serror error ",
    // Console
    "crlf color comment ",
    "computer lanroot ",
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
        // L_TEX vs L_LATEX disambiguation: `.tex` → L_TEX (plain),
        // `.latex` → L_LATEX. The empty-keywords decision in
        // TEX_THEME hinges on this being the case (L_TEX must
        // tolerate LaTeX content too — see scintilla-sys LexTeX
        // banner for the rationale).
        assert_eq!(LangType::from_extension("tex"), L_TEX);
        assert_eq!(LangType::from_extension("latex"), L_LATEX);
        // `.sty` / `.cls` / `.ltx` / `.dtx` — all LaTeX grammar.
        assert_eq!(LangType::from_extension("sty"), L_LATEX);
        assert_eq!(LangType::from_extension("cls"), L_LATEX);
        assert_eq!(LangType::from_extension("ltx"), L_LATEX);
        assert_eq!(LangType::from_extension("dtx"), L_LATEX);
        // Shell dialects all route to L_BASH — LexBash handles
        // their lexical surface for syntax-highlighting purposes.
        assert_eq!(LangType::from_extension("sh"), L_BASH);
        assert_eq!(LangType::from_extension("bash"), L_BASH);
        assert_eq!(LangType::from_extension("ksh"), L_BASH);
        assert_eq!(LangType::from_extension("zsh"), L_BASH);
        assert_eq!(LangType::from_extension("ash"), L_BASH);
        assert_eq!(LangType::from_extension("dash"), L_BASH);
        // TCL family — `.tcl`, `.tk`, `.itcl`, `.exp` (Expect),
        // `.wfs` (Tcl/Tk widget framework). Same lexical surface.
        assert_eq!(LangType::from_extension("tcl"), L_TCL);
        assert_eq!(LangType::from_extension("tk"), L_TCL);
        assert_eq!(LangType::from_extension("itcl"), L_TCL);
        assert_eq!(LangType::from_extension("exp"), L_TCL);
        assert_eq!(LangType::from_extension("wfs"), L_TCL);
        // Lisp family — `.lisp`, `.lsp`, `.el` (Emacs Lisp), `.cl`
        // (ANSI Common Lisp). All share the `lisp` Lexilla lexer.
        // `.scm` / `.ss` / `.sld` / `.sls` route to L_SCHEME below.
        assert_eq!(LangType::from_extension("lisp"), L_LISP);
        assert_eq!(LangType::from_extension("lsp"), L_LISP);
        assert_eq!(LangType::from_extension("el"), L_LISP);
        assert_eq!(LangType::from_extension("cl"), L_LISP);
        // Scheme family — `.scm`, `.ss` (PLT/Racket/Chez), R7RS `.sld`
        // (library definition), R6RS `.sls` (library source). Shares
        // the `lisp` Lexilla lexer with L_LISP but installs distinct
        // SCHEME_KEYWORDS / SCHEME_KEYWORDS_KW per SCHEME_THEME in
        // ui_win32/src/lib.rs. `.rkt` (Racket) NOT included — Racket
        // has diverged from R7RS; a future L_RACKET row is the right
        // destination. `.sps` (R6RS program script) NOT included —
        // vanishingly rare compared to `.sls`; add later if requested.
        assert_eq!(LangType::from_extension("scm"), L_SCHEME);
        assert_eq!(LangType::from_extension("ss"), L_SCHEME);
        assert_eq!(LangType::from_extension("sld"), L_SCHEME);
        assert_eq!(LangType::from_extension("sls"), L_SCHEME);
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

    /// Shell-rc and login-script filenames resolve to `L_BASH`
    /// via the `LangEntry::filenames` lookup path, matching the
    /// `L_MAKEFILE` precedent. `PKGBUILD` (Arch's package script)
    /// and `configure` (autoconf bootstrap) are also pure
    /// Bash / POSIX shell — wired by filename, not extension.
    #[test]
    fn from_path_recognises_bash_by_filename() {
        for name in [
            ".bashrc",
            ".bash_profile",
            ".bash_login",
            ".bash_logout",
            ".bash_aliases",
            ".profile",
            ".zshrc",
            ".zprofile",
            ".zlogin",
            ".zlogout",
            ".zshenv",
            ".kshrc",
            "PKGBUILD",
            "configure",
        ] {
            assert_eq!(
                LangType::from_path(Path::new(name)),
                L_BASH,
                "{name} must route to L_BASH via the filenames lookup"
            );
        }
        // Works through directory paths.
        assert_eq!(LangType::from_path(Path::new("/home/user/.bashrc")), L_BASH);
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
