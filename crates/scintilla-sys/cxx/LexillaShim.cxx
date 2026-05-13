// LexillaShim.cxx — slim drop-in replacement for vendor/lexilla/src/Lexilla.cxx.
//
// Why this file exists.
//   Upstream Lexilla.cxx hardcodes `extern const LexerModule lmXxx;`
//   declarations for every lexer in vendor/lexilla/lexers/, then
//   references all of them in `AddEachLexer()`. Linking succeeds only
//   if every referenced `Lex*.cxx` is also compiled and linked. Code++
//   intentionally compiles a small subset (see build.rs) to bound
//   binary size; that subset would fail to link against the upstream
//   Lexilla.cxx with ~120 unresolved-symbol errors.
//
// Maintenance contract.
//   The forward-declaration block and the AddEachLexer initializer
//   list MUST stay in sync with the `&[Lex*]` array in
//   `crates/scintilla-sys/build.rs`. Adding a lexer is a two-step
//   change: append the file in build.rs AND append `extern const
//   LexerModule lmXxx;` plus `&lmXxx,` here. Removing a lexer is the
//   reverse.
//
//   **Asymmetric failure mode (read this).** Build-step asymmetry:
//
//     - Referenced here but NOT compiled in build.rs → link fails
//       fast with an unresolved-symbol error (good — easy to catch).
//     - Compiled in build.rs but NOT referenced here → build
//       succeeds, but `CreateLexer("foo")` returns nullptr at
//       runtime because the catalog never sees `lmFoo`. The lexer
//       is silently disabled (bad — looks linked, isn't usable).
//
//   The second case is the one that bit us: lexers added to
//   build.rs without their `extern const LexerModule` + catalog
//   entry here behave as `🟡 lexer attached, no host theme` in
//   `docs/lexers-coverage.md`'s sense even though they're not
//   actually attached. Cross-check both sides on every lexer add.
//
//   No other source is copied from upstream. The C entry-point
//   bodies are written here from scratch but mirror the public ABI
//   in `vendor/lexilla/include/Lexilla.h`; the four-line strcmp
//   loop in CreateLexer is the documented contract, not a copy.

#include <cstring>

#include <vector>
#include <initializer_list>

#if defined(_WIN32)
#define EXPORT_FUNCTION __declspec(dllexport)
#define CALLING_CONVENTION __stdcall
#else
#define EXPORT_FUNCTION __attribute__((visibility("default")))
#define CALLING_CONVENTION
#endif

#include "ILexer.h"

#include "LexerModule.h"
#include "CatalogueModules.h"

using namespace Lexilla;

// Lexers we statically link. Each `lmXxx` is defined at global scope
// inside its `Lex*.cxx` file (under `using namespace Lexilla;`), not
// inside `namespace Lexilla`, so the forward declarations here also
// live at global scope to match. Adding a new one here also requires
// the matching `Lex*.cxx` in build.rs's lexer list — see file header.
//
// Important: a Lex*.cxx file may register **multiple** `LexerModule`
// globals. `LexHTML.cxx` is the prime example — it defines `lmHTML`
// (name "hypertext"), `lmXML` (name "xml"), and `lmPHPSCRIPT` (name
// "phpscript"). Each named lexer needs its own forward-decl + catalog
// entry below; missing one silently disables that lexer at runtime
// even though `Lex*.cxx` is in the compile list (the `lm*` global is
// defined but the catalog never sees it, so `CreateLexer(name)`
// returns `nullptr` and the host falls through to `clear_lexer`).
//
// Entries are grouped by source `Lex*.cxx` file, ordered by ASCII
// byte value on the file name — so `LexCSS` sorts before `LexCaml`
// (uppercase `S` < lowercase `a` in ASCII) and `LexHTML` before
// `LexHaskell`. Case-insensitive alphabetisation would shuffle a
// handful of entries; ASCII ordering is what the existing block
// uses and what `grep -nE "^extern const LexerModule" Lex*.cxx`
// also produces, so cross-referencing against the source set is a
// straight visual diff. The lookup order is irrelevant to runtime
// (CatalogueModules does a linear strcmp scan); only readability
// matters.
//
// The Lexilla-exposed lexer name is on each line as a trailing
// comment — that's the string `set_lexer_by_name` matches against
// (and the same string used by `LANG_TABLE`'s `lexer:` field in
// `core::lang`).
extern const LexerModule lmAda;         // "ada" — LexAda.cxx
extern const LexerModule lmAsm;         // "asm" — LexAsm.cxx (primary, `;` comments)
extern const LexerModule lmAs;          // "as" — LexAsm.cxx (secondary asm flavour, `#` comments — see `LexerFactoryAs` at LexAsm.cxx:223)
extern const LexerModule lmAsn1;        // "asn1" — LexAsn1.cxx
extern const LexerModule lmAU3;         // "au3" — LexAU3.cxx (AutoIt)
extern const LexerModule lmAVS;         // "avs" — LexAVS.cxx (AviSynth)
extern const LexerModule lmBaan;        // "baan" — LexBaan.cxx
extern const LexerModule lmBash;        // "bash" — LexBash.cxx
extern const LexerModule lmBlitzBasic;  // "blitzbasic" — LexBasic.cxx
extern const LexerModule lmPureBasic;   // "purebasic" — LexBasic.cxx
extern const LexerModule lmFreeBasic;   // "freebasic" — LexBasic.cxx
extern const LexerModule lmBatch;       // "batch" — LexBatch.cxx
extern const LexerModule lmCOBOL;       // "COBOL" — LexCOBOL.cxx
extern const LexerModule lmCPP;         // "cpp" — LexCPP.cxx (C / C++ / Java / JS / TS / Obj-C / Go / Swift / RC / C#)
extern const LexerModule lmCPPNoCase;   // "cppnocase" — LexCPP.cxx (case-insensitive sibling)
extern const LexerModule lmCss;         // "css" — LexCSS.cxx (note the lowercase `lmCss`)
extern const LexerModule lmCaml;        // "caml" — LexCaml.cxx
extern const LexerModule lmCmake;       // "cmake" — LexCmake.cxx
extern const LexerModule lmCoffeeScript; // "coffeescript" — LexCoffeeScript.cxx
extern const LexerModule lmNncrontab;   // "nncrontab" — LexCrontab.cxx
extern const LexerModule lmCsound;      // "csound" — LexCsound.cxx
extern const LexerModule lmD;           // "d" — LexD.cxx
extern const LexerModule lmDiff;        // "diff" — LexDiff.cxx
extern const LexerModule lmErlang;      // "erlang" — LexErlang.cxx
extern const LexerModule lmErrorList;   // "errorlist" — LexErrorList.cxx
extern const LexerModule lmESCRIPT;     // "escript" — LexEScript.cxx
extern const LexerModule lmForth;       // "forth" — LexForth.cxx
extern const LexerModule lmFortran;     // "fortran" — LexFortran.cxx (free form)
extern const LexerModule lmF77;         // "f77" — LexFortran.cxx (fixed form)
extern const LexerModule lmGDScript;    // "gdscript" — LexGDScript.cxx
extern const LexerModule lmGui4Cli;     // "gui4cli" — LexGui4Cli.cxx
extern const LexerModule lmHTML;        // "hypertext" — LexHTML.cxx (HTML / ASP / JSP / PHP)
extern const LexerModule lmXML;         // "xml" — LexHTML.cxx
extern const LexerModule lmPHPSCRIPT;   // "phpscript" — LexHTML.cxx (pure PHP, no HTML wrapper)
extern const LexerModule lmHaskell;     // "haskell" — LexHaskell.cxx
extern const LexerModule lmLiterateHaskell; // "literatehaskell" — LexHaskell.cxx
extern const LexerModule lmSrec;        // "srec" — LexHex.cxx (Motorola S-Record)
extern const LexerModule lmIHex;        // "ihex" — LexHex.cxx (Intel HEX)
extern const LexerModule lmTEHex;       // "tehex" — LexHex.cxx (Tektronix extended HEX)
extern const LexerModule lmHollywood;   // "hollywood" — LexHollywood.cxx
extern const LexerModule lmInno;        // "inno" — LexInno.cxx (Inno Setup)
extern const LexerModule lmJSON;        // "json" — LexJSON.cxx
extern const LexerModule lmKix;         // "kix" — LexKix.cxx (KIXtart)
extern const LexerModule lmLatex;       // "latex" — LexLaTeX.cxx
extern const LexerModule lmLISP;        // "lisp" — LexLisp.cxx (also used for Scheme)
extern const LexerModule lmLua;         // "lua" — LexLua.cxx
extern const LexerModule lmMMIXAL;      // "mmixal" — LexMMIXAL.cxx
extern const LexerModule lmMSSQL;       // "mssql" — LexMSSQL.cxx (kept on hand for future MS T-SQL row)
extern const LexerModule lmMake;        // "makefile" — LexMake.cxx
extern const LexerModule lmMatlab;      // "matlab" — LexMatlab.cxx
extern const LexerModule lmOctave;      // "octave" — LexMatlab.cxx
extern const LexerModule lmNim;         // "nim" — LexNim.cxx
extern const LexerModule lmNsis;        // "nsis" — LexNsis.cxx
extern const LexerModule lmNull;        // "null" — LexNull.cxx (built-in plain-text fallback)
extern const LexerModule lmOScript;     // "oscript" — LexOScript.cxx
extern const LexerModule lmPS;          // "ps" — LexPS.cxx (PostScript)
extern const LexerModule lmPascal;      // "pascal" — LexPascal.cxx
extern const LexerModule lmPerl;        // "perl" — LexPerl.cxx
extern const LexerModule lmPowerShell;  // "powershell" — LexPowerShell.cxx
extern const LexerModule lmProps;       // "props" — LexProps.cxx (properties / INI)
extern const LexerModule lmPython;      // "python" — LexPython.cxx
extern const LexerModule lmR;           // "r" — LexR.cxx
extern const LexerModule lmRaku;        // "raku" — LexRaku.cxx
extern const LexerModule lmREBOL;       // "rebol" — LexRebol.cxx
extern const LexerModule lmRegistry;    // "registry" — LexRegistry.cxx
extern const LexerModule lmRuby;        // "ruby" — LexRuby.cxx
extern const LexerModule lmRust;        // "rust" — LexRust.cxx
extern const LexerModule lmSAS;         // "sas" — LexSAS.cxx
extern const LexerModule lmSQL;         // "sql" — LexSQL.cxx
extern const LexerModule lmSmalltalk;   // "smalltalk" — LexSmalltalk.cxx
extern const LexerModule lmSpice;       // "spice" — LexSpice.cxx
extern const LexerModule lmTCL;         // "tcl" — LexTCL.cxx
extern const LexerModule lmTOML;        // "toml" — LexTOML.cxx
extern const LexerModule lmTeX;         // "tex" — LexTeX.cxx
extern const LexerModule lmTxt2tags;    // "txt2tags" — LexTxt2tags.cxx
extern const LexerModule lmVB;          // "vb" — LexVB.cxx
extern const LexerModule lmVBScript;    // "vbscript" — LexVB.cxx
extern const LexerModule lmVHDL;        // "vhdl" — LexVHDL.cxx
extern const LexerModule lmVerilog;     // "verilog" — LexVerilog.cxx
extern const LexerModule lmVisualProlog; // "visualprolog" — LexVisualProlog.cxx
extern const LexerModule lmYAML;        // "yaml" — LexYAML.cxx

static CatalogueModules catalogueLexilla;

static void AddEachLexer() {
    if (catalogueLexilla.Count() != 0) {
        return;
    }
    // Same ordering as the extern block above — grouped by Lex*.cxx
    // file alphabetically. The order here is irrelevant to lookup
    // (CatalogueModules does a linear strcmp scan in CreateLexer)
    // but the alphabetical-by-source-file shape lets a reader pair
    // an entry here with its forward declaration above.
    catalogueLexilla.AddLexerModules({
        &lmAda,
        &lmAsm, &lmAs,
        &lmAsn1,
        &lmAU3,
        &lmAVS,
        &lmBaan,
        &lmBash,
        &lmBlitzBasic, &lmPureBasic, &lmFreeBasic,
        &lmBatch,
        &lmCOBOL,
        &lmCPP, &lmCPPNoCase,
        &lmCss,
        &lmCaml,
        &lmCmake,
        &lmCoffeeScript,
        &lmNncrontab,
        &lmCsound,
        &lmD,
        &lmDiff,
        &lmErlang,
        &lmErrorList,
        &lmESCRIPT,
        &lmForth,
        &lmFortran, &lmF77,
        &lmGDScript,
        &lmGui4Cli,
        &lmHTML, &lmXML, &lmPHPSCRIPT,
        &lmHaskell, &lmLiterateHaskell,
        &lmSrec, &lmIHex, &lmTEHex,
        &lmHollywood,
        &lmInno,
        &lmJSON,
        &lmKix,
        &lmLatex,
        &lmLISP,
        &lmLua,
        &lmMMIXAL,
        &lmMSSQL,
        &lmMake,
        &lmMatlab, &lmOctave,
        &lmNim,
        &lmNsis,
        &lmNull,
        &lmOScript,
        &lmPS,
        &lmPascal,
        &lmPerl,
        &lmPowerShell,
        &lmProps,
        &lmPython,
        &lmR,
        &lmRaku,
        &lmREBOL,
        &lmRegistry,
        &lmRuby,
        &lmRust,
        &lmSAS,
        &lmSQL,
        &lmSmalltalk,
        &lmSpice,
        &lmTCL,
        &lmTOML,
        &lmTeX,
        &lmTxt2tags,
        &lmVB, &lmVBScript,
        &lmVHDL,
        &lmVerilog,
        &lmVisualProlog,
        &lmYAML,
    });
}

extern "C" {

EXPORT_FUNCTION int CALLING_CONVENTION GetLexerCount() {
    AddEachLexer();
    return static_cast<int>(catalogueLexilla.Count());
}

EXPORT_FUNCTION void CALLING_CONVENTION GetLexerName(unsigned int index, char *name, int buflength) {
    // `buflength` is `int` per the public ABI in Lexilla.h. A loaded
    // plugin (the only mover of this entry point — exported symbols
    // are visible to plugin DLLs that resolve them by name from the
    // host module) calling this with a non-positive `buflength`
    // would, after the documented `static_cast<size_t>(buflength)`
    // promotion, produce a huge size_t and let an unbounded `strcpy`
    // run. Upstream `vendor/lexilla/src/Lexilla.cxx` mirrors this
    // signed-int parameter and inherits the same edge case; we
    // close it explicitly here because the shim's `EXPORT_FUNCTION`
    // makes the symbol cross-module-callable.
    if (buflength <= 0 || name == nullptr) {
        return;
    }
    AddEachLexer();
    *name = 0;
    const char *lexerName = catalogueLexilla.Name(index);
    if (lexerName != nullptr && static_cast<size_t>(buflength) > strlen(lexerName)) {
        strcpy(name, lexerName);
    }
}

EXPORT_FUNCTION LexerFactoryFunction CALLING_CONVENTION GetLexerFactory(unsigned int index) {
    AddEachLexer();
    return catalogueLexilla.Factory(index);
}

EXPORT_FUNCTION Scintilla::ILexer5 * CALLING_CONVENTION CreateLexer(const char *name) {
    AddEachLexer();
    for (size_t i = 0; i < catalogueLexilla.Count(); i++) {
        const char *lexerName = catalogueLexilla.Name(i);
        if (0 == strcmp(lexerName, name)) {
            return catalogueLexilla.Create(i);
        }
    }
    return nullptr;
}

EXPORT_FUNCTION const char * CALLING_CONVENTION LexerNameFromID(int identifier) {
    AddEachLexer();
    const LexerModule *pModule = catalogueLexilla.Find(identifier);
    if (pModule) {
        return pModule->languageName;
    }
    return nullptr;
}

EXPORT_FUNCTION const char * CALLING_CONVENTION GetLibraryPropertyNames() {
    return "";
}

EXPORT_FUNCTION void CALLING_CONVENTION SetLibraryProperty(const char *, const char *) {
    // Null implementation — Code++ doesn't expose Lexilla properties yet.
}

EXPORT_FUNCTION const char * CALLING_CONVENTION GetNameSpace() {
    return "lexilla";
}

}

// Not exported from binary; symmetrical with upstream so any future
// in-tree code that wants to register a Code++-internal lexer can call
// it without surprises.
void AddStaticLexerModule(const LexerModule *plm) {
    AddEachLexer();
    catalogueLexilla.AddLexerModule(plm);
}
