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
//   reverse. The `cargo build` link step is the enforcement gate —
//   miss either side and the build fails fast.
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
extern const LexerModule lmCPP;
extern const LexerModule lmNull;
extern const LexerModule lmRust;

static CatalogueModules catalogueLexilla;

static void AddEachLexer() {
    if (catalogueLexilla.Count() != 0) {
        return;
    }
    catalogueLexilla.AddLexerModules({
        &lmCPP,
        &lmNull,
        &lmRust,
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
