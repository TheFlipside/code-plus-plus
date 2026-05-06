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
    pub fn as_npp_id(self) -> i32 {
        self.0
    }

    /// Resolve a file extension (without the leading dot, lower-cased
    /// or not — we lower-case internally) to a known `LangType`. Falls
    /// back to [`L_TEXT`] for anything we don't recognise.
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

    /// Resolve a path to a `LangType` by inspecting its extension.
    /// Files with no extension (or an empty one) return [`L_TEXT`].
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|s| s.to_str()) {
            Some(ext) => Self::from_extension(ext),
            None => L_TEXT,
        }
    }

    /// The string Lexilla expects in `CreateLexer(name)`. Returns
    /// `None` for [`L_TEXT`] (no lexer attached — Scintilla renders
    /// the buffer in the default style) and for any LangType not in
    /// the table (a plugin might set a future N++ enum value via
    /// `NPPM_SETBUFFERLANGTYPE`).
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
    pub fn language_name(self) -> Option<&'static str> {
        LANG_TABLE
            .iter()
            .find(|e| e.lang == self)
            .map(|e| e.menu_label)
    }

    /// Long human-readable description returned by `NPPM_GETLANGUAGEDESC`.
    /// Notepad++ uses the longer phrasing here ("C++ source file");
    /// plugins display it in language-pickers and about-dialogs.
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
    /// either L_TEXT (we want no lexer) or "no Lexilla lexer is the
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
    },
    LangEntry {
        lang: L_ASN1,
        menu_label: "ASN.1",
        desc: "ASN.1 source file",
        lexer: Some("asn1"),
        extensions: &["asn1"],
    },
    LangEntry {
        lang: L_ASP,
        menu_label: "ASP",
        desc: "ASP source file",
        lexer: Some("hypertext"),
        extensions: &["asp"],
    },
    LangEntry {
        lang: L_ASM,
        menu_label: "Assembly",
        desc: "Assembly source file",
        lexer: Some("asm"),
        extensions: &["asm", "s"],
    },
    LangEntry {
        lang: L_AU3,
        menu_label: "AutoIt",
        desc: "AutoIt source file",
        lexer: Some("au3"),
        extensions: &["au3"],
    },
    LangEntry {
        lang: L_AVS,
        menu_label: "AviSynth",
        desc: "AviSynth source file",
        lexer: Some("avs"),
        extensions: &["avs", "avsi"],
    },
    LangEntry {
        lang: L_BAANC,
        menu_label: "BaanC",
        desc: "BaanC source file",
        lexer: Some("baan"),
        extensions: &["baan"],
    },
    LangEntry {
        lang: L_BATCH,
        menu_label: "Batch",
        desc: "Batch file",
        lexer: Some("batch"),
        extensions: &["bat", "cmd"],
    },
    LangEntry {
        lang: L_BLITZBASIC,
        menu_label: "Blitzbasic",
        desc: "Blitzbasic source file",
        lexer: Some("blitzbasic"),
        extensions: &["bb"],
    },
    LangEntry {
        lang: L_C,
        menu_label: "C",
        desc: "C source file",
        lexer: Some("cpp"),
        extensions: &["c", "h"],
    },
    LangEntry {
        lang: L_CS,
        menu_label: "C#",
        desc: "C# source file",
        lexer: Some("cpp"),
        extensions: &["cs"],
    },
    LangEntry {
        lang: L_CPP,
        menu_label: "C++",
        desc: "C++ source file",
        lexer: Some("cpp"),
        extensions: &["cpp", "cxx", "cc", "hpp", "hxx", "hh", "ipp", "tpp", "inl"],
    },
    LangEntry {
        lang: L_CAML,
        menu_label: "Caml",
        desc: "Caml source file",
        lexer: Some("caml"),
        extensions: &["ml", "mli"],
    },
    LangEntry {
        lang: L_CMAKE,
        menu_label: "CMake",
        desc: "CMake source file",
        lexer: Some("cmake"),
        extensions: &["cmake"],
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
    },
    LangEntry {
        lang: L_COFFEESCRIPT,
        menu_label: "CoffeeScript",
        desc: "CoffeeScript source file",
        lexer: Some("coffeescript"),
        extensions: &["coffee", "litcoffee"],
    },
    LangEntry {
        lang: L_CSOUND,
        menu_label: "CSound",
        desc: "CSound source file",
        lexer: Some("csound"),
        extensions: &["orc", "sco", "csd"],
    },
    LangEntry {
        lang: L_CSS,
        menu_label: "CSS",
        desc: "CSS source file",
        lexer: Some("css"),
        extensions: &["css"],
    },
    LangEntry {
        lang: L_D,
        menu_label: "D",
        desc: "D source file",
        lexer: Some("d"),
        extensions: &["d"],
    },
    LangEntry {
        lang: L_DIFF,
        menu_label: "Diff",
        desc: "Diff/patch file",
        lexer: Some("diff"),
        extensions: &["diff", "patch"],
    },
    LangEntry {
        lang: L_ERLANG,
        menu_label: "Erlang",
        desc: "Erlang source file",
        lexer: Some("erlang"),
        extensions: &["erl", "hrl"],
    },
    LangEntry {
        lang: L_ERRORLIST,
        menu_label: "ErrorList",
        desc: "Error-list output file",
        lexer: Some("errorlist"),
        extensions: &[],
    },
    LangEntry {
        lang: L_ESCRIPT,
        menu_label: "ESCRIPT",
        desc: "ESCRIPT source file",
        lexer: Some("escript"),
        extensions: &["em"],
    },
    LangEntry {
        lang: L_FORTH,
        menu_label: "Forth",
        desc: "Forth source file",
        lexer: Some("forth"),
        extensions: &["forth"],
    },
    LangEntry {
        lang: L_FORTRAN_77,
        menu_label: "Fortran (fixed form)",
        desc: "Fortran (fixed form) source file",
        lexer: Some("f77"),
        extensions: &["f", "for", "f77", "ftn"],
    },
    LangEntry {
        lang: L_FORTRAN,
        menu_label: "Fortran (free form)",
        desc: "Fortran (free form) source file",
        lexer: Some("fortran"),
        extensions: &["f90", "f95", "f2k", "f03", "f08", "f15"],
    },
    LangEntry {
        lang: L_FREEBASIC,
        menu_label: "Freebasic",
        desc: "Freebasic source file",
        lexer: Some("freebasic"),
        extensions: &["bas"],
    },
    LangEntry {
        lang: L_GDSCRIPT,
        menu_label: "GDScript",
        desc: "GDScript source file",
        lexer: Some("gdscript"),
        extensions: &["gd"],
    },
    LangEntry {
        lang: L_GOLANG,
        menu_label: "Go",
        desc: "Go source file",
        lexer: Some("cpp"),
        extensions: &["go"],
    },
    LangEntry {
        lang: L_GUI4CLI,
        menu_label: "Gui4Cli",
        desc: "Gui4Cli source file",
        lexer: Some("gui4cli"),
        extensions: &["gc", "gui"],
    },
    LangEntry {
        lang: L_HASKELL,
        menu_label: "Haskell",
        desc: "Haskell source file",
        lexer: Some("haskell"),
        extensions: &["hs"],
    },
    LangEntry {
        lang: L_HOLLYWOOD,
        menu_label: "Hollywood",
        desc: "Hollywood source file",
        lexer: Some("hollywood"),
        extensions: &["hws"],
    },
    LangEntry {
        lang: L_HTML,
        menu_label: "HTML",
        desc: "HTML file",
        lexer: Some("hypertext"),
        extensions: &["html", "htm", "xhtml"],
    },
    LangEntry {
        lang: L_INI,
        menu_label: "INI file",
        desc: "INI file",
        lexer: Some("props"),
        extensions: &["ini"],
    },
    LangEntry {
        lang: L_INNO,
        menu_label: "Inno Setup",
        desc: "Inno Setup script",
        lexer: Some("inno"),
        extensions: &["iss"],
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
    },
    LangEntry {
        lang: L_JAVA,
        menu_label: "Java",
        desc: "Java source file",
        lexer: Some("cpp"),
        extensions: &["java"],
    },
    LangEntry {
        lang: L_JAVASCRIPT,
        menu_label: "Javascript",
        desc: "Javascript source file",
        lexer: Some("cpp"),
        extensions: &["js", "mjs", "cjs"],
    },
    LangEntry {
        lang: L_JSON,
        menu_label: "JSON",
        desc: "JSON file",
        lexer: Some("json"),
        extensions: &["json"],
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
    },
    LangEntry {
        lang: L_JSP,
        menu_label: "JSP",
        desc: "JSP source file",
        lexer: Some("hypertext"),
        extensions: &["jsp"],
    },
    LangEntry {
        lang: L_KIX,
        menu_label: "KIXtart",
        desc: "KIXtart source file",
        lexer: Some("kix"),
        extensions: &["kix"],
    },
    LangEntry {
        lang: L_LATEX,
        menu_label: "LaTeX",
        desc: "LaTeX source file",
        lexer: Some("latex"),
        extensions: &["latex"],
    },
    LangEntry {
        lang: L_LISP,
        menu_label: "Lisp",
        desc: "Lisp source file",
        lexer: Some("lisp"),
        extensions: &["lisp", "lsp", "el"],
    },
    LangEntry {
        lang: L_LUA,
        menu_label: "Lua",
        desc: "Lua source file",
        lexer: Some("lua"),
        extensions: &["lua"],
    },
    LangEntry {
        lang: L_MAKEFILE,
        menu_label: "Makefile",
        desc: "Makefile",
        lexer: Some("makefile"),
        extensions: &["makefile", "mak", "mk"],
    },
    LangEntry {
        lang: L_MATLAB,
        menu_label: "Matlab",
        desc: "Matlab source file",
        lexer: Some("matlab"),
        extensions: &["matlab"],
    },
    LangEntry {
        lang: L_MMIXAL,
        menu_label: "MMIXAL",
        desc: "MMIXAL source file",
        lexer: Some("mmixal"),
        extensions: &["mms"],
    },
    LangEntry {
        lang: L_NIM,
        menu_label: "Nim",
        desc: "Nim source file",
        lexer: Some("nim"),
        extensions: &["nim"],
    },
    LangEntry {
        lang: L_NNCRONTAB,
        menu_label: "Nncrontab",
        desc: "Nncrontab file",
        lexer: Some("nncrontab"),
        extensions: &["tab"],
    },
    LangEntry {
        lang: L_NSIS,
        menu_label: "NSIS",
        desc: "NSIS script",
        lexer: Some("nsis"),
        extensions: &["nsi", "nsh"],
    },
    LangEntry {
        lang: L_OBJC,
        menu_label: "Objective-C",
        desc: "Objective-C source file",
        lexer: Some("cpp"),
        extensions: &["m", "mm"],
    },
    LangEntry {
        lang: L_OSCRIPT,
        menu_label: "OScript",
        desc: "OScript source file",
        lexer: Some("oscript"),
        extensions: &["osx"],
    },
    LangEntry {
        lang: L_PASCAL,
        menu_label: "Pascal",
        desc: "Pascal source file",
        lexer: Some("pascal"),
        extensions: &["pas", "pp", "p", "dpr"],
    },
    LangEntry {
        lang: L_PERL,
        menu_label: "Perl",
        desc: "Perl source file",
        lexer: Some("perl"),
        extensions: &["pl", "pm", "plx"],
    },
    LangEntry {
        lang: L_PHP,
        menu_label: "PHP",
        desc: "PHP source file",
        lexer: Some("hypertext"),
        extensions: &["php", "phtml"],
    },
    LangEntry {
        lang: L_PS,
        menu_label: "PostScript",
        desc: "PostScript file",
        lexer: Some("ps"),
        extensions: &["ps", "eps"],
    },
    LangEntry {
        lang: L_POWERSHELL,
        menu_label: "PowerShell",
        desc: "PowerShell source file",
        lexer: Some("powershell"),
        extensions: &["ps1", "psm1", "psd1"],
    },
    LangEntry {
        lang: L_PROPS,
        menu_label: "Properties",
        desc: "Properties file",
        lexer: Some("props"),
        extensions: &["properties"],
    },
    LangEntry {
        lang: L_PUREBASIC,
        menu_label: "Purebasic",
        desc: "Purebasic source file",
        lexer: Some("purebasic"),
        extensions: &["pb"],
    },
    LangEntry {
        lang: L_PYTHON,
        menu_label: "Python",
        desc: "Python source file",
        lexer: Some("python"),
        extensions: &["py", "pyw"],
    },
    LangEntry {
        lang: L_R,
        menu_label: "R",
        desc: "R source file",
        lexer: Some("r"),
        extensions: &["r"],
    },
    LangEntry {
        lang: L_RAKU,
        menu_label: "Raku",
        desc: "Raku source file",
        lexer: Some("raku"),
        extensions: &["raku", "rakumod"],
    },
    LangEntry {
        lang: L_REBOL,
        menu_label: "REBOL",
        desc: "REBOL source file",
        lexer: Some("rebol"),
        extensions: &["reb", "rebol"],
    },
    LangEntry {
        lang: L_REGISTRY,
        menu_label: "Registry",
        desc: "Windows Registry file",
        lexer: Some("registry"),
        extensions: &["reg"],
    },
    LangEntry {
        lang: L_RC,
        menu_label: "Resource file",
        desc: "Resource source file",
        lexer: Some("cpp"),
        extensions: &["rc"],
    },
    LangEntry {
        lang: L_RUBY,
        menu_label: "Ruby",
        desc: "Ruby source file",
        lexer: Some("ruby"),
        extensions: &["rb", "rbw"],
    },
    LangEntry {
        lang: L_RUST,
        menu_label: "Rust",
        desc: "Rust source file",
        lexer: Some("rust"),
        extensions: &["rs"],
    },
    LangEntry {
        lang: L_SREC,
        menu_label: "S-Record",
        desc: "Motorola S-Record file",
        lexer: Some("srec"),
        extensions: &["srec", "s19", "s28", "s37"],
    },
    LangEntry {
        lang: L_SAS,
        menu_label: "SAS",
        desc: "SAS source file",
        lexer: Some("sas"),
        extensions: &["sas"],
    },
    LangEntry {
        lang: L_SCHEME,
        menu_label: "Scheme",
        desc: "Scheme source file",
        lexer: Some("lisp"),
        extensions: &["scm", "ss"],
    },
    LangEntry {
        lang: L_BASH,
        menu_label: "Shell",
        desc: "Shell script",
        lexer: Some("bash"),
        extensions: &["sh", "bash"],
    },
    LangEntry {
        lang: L_SMALLTALK,
        menu_label: "Smalltalk",
        desc: "Smalltalk source file",
        lexer: Some("smalltalk"),
        extensions: &["st"],
    },
    LangEntry {
        lang: L_SPICE,
        menu_label: "Spice",
        desc: "Spice circuit file",
        lexer: Some("spice"),
        extensions: &["sp", "spice"],
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
    },
    LangEntry {
        lang: L_SWIFT,
        menu_label: "Swift",
        desc: "Swift source file",
        lexer: Some("cpp"),
        extensions: &["swift"],
    },
    LangEntry {
        lang: L_TCL,
        menu_label: "TCL",
        desc: "TCL source file",
        lexer: Some("tcl"),
        extensions: &["tcl"],
    },
    LangEntry {
        lang: L_TEHEX,
        menu_label: "Tektronix extended HEX",
        desc: "Tektronix Extended HEX file",
        lexer: Some("tehex"),
        extensions: &["tek"],
    },
    LangEntry {
        lang: L_TEX,
        menu_label: "TeX",
        desc: "TeX source file",
        lexer: Some("tex"),
        extensions: &["tex"],
    },
    LangEntry {
        lang: L_TOML,
        menu_label: "TOML",
        desc: "TOML file",
        lexer: Some("toml"),
        extensions: &["toml"],
    },
    LangEntry {
        lang: L_TXT2TAGS,
        menu_label: "txt2tags",
        desc: "txt2tags source file",
        lexer: Some("txt2tags"),
        extensions: &["t2t"],
    },
    LangEntry {
        lang: L_TYPESCRIPT,
        menu_label: "TypeScript",
        desc: "TypeScript source file",
        lexer: Some("cpp"),
        extensions: &["ts", "tsx"],
    },
    LangEntry {
        lang: L_VERILOG,
        menu_label: "Verilog",
        desc: "Verilog source file",
        lexer: Some("verilog"),
        extensions: &["v", "vh", "sv", "svh"],
    },
    LangEntry {
        lang: L_VHDL,
        menu_label: "VHDL",
        desc: "VHDL source file",
        lexer: Some("vhdl"),
        extensions: &["vhd", "vhdl"],
    },
    LangEntry {
        lang: L_VB,
        menu_label: "Visual Basic",
        desc: "Visual Basic source file",
        lexer: Some("vb"),
        extensions: &["vb", "vbs"],
    },
    LangEntry {
        lang: L_VISUALPROLOG,
        menu_label: "Visual Prolog",
        desc: "Visual Prolog source file",
        lexer: Some("visualprolog"),
        extensions: &["vip"],
    },
    LangEntry {
        lang: L_XML,
        menu_label: "XML",
        desc: "XML file",
        lexer: Some("xml"),
        extensions: &["xml", "xsd", "xsl", "xslt", "svg"],
    },
    LangEntry {
        lang: L_YAML,
        menu_label: "YAML",
        desc: "YAML file",
        lexer: Some("yaml"),
        extensions: &["yaml", "yml"],
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

/// Space-separated keyword list installed via `SCI_SETKEYWORDS(0, ...)`
/// when the active language is C. Keeps the demo-gate `.c` file showing
/// keywords coloured even though LexCPP's default keyword set is
/// empty. Includes C99/C11 keywords; not exhaustive but covers the
/// "see colour on `int`/`return`/`if`" case.
pub const C_KEYWORDS: &str = concat!(
    "auto break case char const continue default do double else enum extern ",
    "float for goto if inline int long register restrict return short signed ",
    "sizeof static struct switch typedef union unsigned void volatile while ",
    "_Bool _Complex _Imaginary _Alignas _Alignof _Atomic _Generic _Noreturn ",
    "_Static_assert _Thread_local"
);

/// Space-separated keyword list for C++. Superset of [`C_KEYWORDS`]
/// plus the C++23-and-earlier reserved words.
pub const CPP_KEYWORDS: &str = concat!(
    "alignas alignof and and_eq asm auto bitand bitor bool break case catch ",
    "char char8_t char16_t char32_t class compl concept const consteval ",
    "constexpr constinit const_cast continue co_await co_return co_yield ",
    "decltype default delete do double dynamic_cast else enum explicit export ",
    "extern false float for friend goto if inline int long mutable namespace ",
    "new noexcept not not_eq nullptr operator or or_eq private protected ",
    "public register reinterpret_cast requires return short signed sizeof ",
    "static static_assert static_cast struct switch template this thread_local ",
    "throw true try typedef typeid typename union unsigned using virtual void ",
    "volatile wchar_t while xor xor_eq"
);

/// Space-separated primary-keyword list for Rust. LexRust's keyword
/// classes 0 = primary, 1 = secondary; we install just primary at m1.
pub const RUST_KEYWORDS: &str = concat!(
    "as async await break const continue crate dyn else enum extern false fn ",
    "for if impl in let loop match mod move mut pub ref return self Self ",
    "static struct super trait true try type union unsafe use where while"
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
        assert_eq!(LangType::from_path(Path::new("Makefile")), L_TEXT);
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
        ids.sort();
        for window in ids.windows(2) {
            assert!(
                window[0] != window[1],
                "duplicate LangType in LANG_TABLE: {}",
                window[0]
            );
        }
    }
}
