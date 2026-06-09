# Lexer host-side wiring coverage matrix

Authoritative source for which `core::lang::LANG_TABLE` entries
have **keyword lists** and **per-style colour themes** wired up
on Code++'s side. Updated on every commit that adds, expands, or
deprecates a lexer's host-side configuration.

This is **not** about whether the Lexilla lexer is statically
linked into the binary — that's tracked separately in
`crates/scintilla-sys/build.rs`'s lexer list. Every row below
whose `LANG_TABLE` entry has `lexer: Some(_)` already has its
lexer linked. The matrix below reports what the **host**
contributes on top of that:

1. **Keywords** — does Code++ call `SCI_SETKEYWORDS(class, words)`
   so the lexer can distinguish keywords from plain identifiers?
2. **Theme** — does Code++ call `SCI_STYLESETFORE` (and bold /
   italic / back where appropriate) for each of the lexer's
   `SCE_*_*` style indices, so comments / strings / numbers /
   keywords pick up distinct visible colours?

Without **both** of those, the lexer tokenises correctly but
every classification renders at `STYLE_DEFAULT` (black on white),
so the user sees no highlighting even though the lexer is
running.

See DESIGN.md §7.2 Phase 4.5 for the binding completion gate
(this matrix at ≥80% ✅ before the phase ships, with the
residual rows formally tracked).

## Status legend

| Glyph | Meaning |
| --- | --- |
| ✅ | Keywords + theme both wired in `Win32Ui::apply_lang`'s table. Pick this language from the Language menu and a sample file picks up visibly distinct colours for comments, strings, numbers, and keywords. |
| 🟡 | Lexer attached and tokenising; no host keyword list and no host theme. Buffer renders uniformly black-on-white because every `SCE_*_*` style resolves to `STYLE_DEFAULT` after `SCI_STYLECLEARALL`. (Pre-2026-05-13: this row also covered "lexer compiled but unregistered in `LexillaShim.cxx`'s catalog"; that gap is now closed for every `LANG_TABLE` row with a non-`None` lexer.) |
| ⚫ | No Lexilla lexer (`LANG_TABLE` row has `lexer: None`). Either by design (`L_TEXT` — plain text never highlights) or because no Lexilla lexer matches the language. Effectively a permanent state for the named row. |
| ⏸ | Reserved for future host-side opt-out (e.g. a lexer the host deliberately leaves off the menu pending review). None today. |

## How to mark a row ✅

The Phase 4.5 framework lives in
[`crates/ui_win32/src/lib.rs`](../crates/ui_win32/src/lib.rs)
under the "Phase 4.5 — table-driven language theme framework"
banner. `Win32Ui::apply_lang` dispatches through
`lang_theme(LangType) -> Option<&'static LangTheme>` — adding
a row means one `else if` branch in that function plus a small
data block of consts (keywords, styles, italic, bold, theme).

For each new language:

1. **SCE_* constants.** Confirm the lexer's `SCE_*_*` style
   constants are declared in
   [`crates/scintilla-sys/src/lib.rs`](../crates/scintilla-sys/src/lib.rs).
   The Phase 4.5 starter set (Python, JSON, Bash, Lua, SQL,
   YAML, TOML, CSS) is already there as scaffolding; new
   lexers append a batch with a comment citing the upstream
   `vendor/lexilla/include/SciLexer.h` line range.
2. **Keyword list.** Author a `<LANG>_KEYWORDS: &str` const in
   `core::lang` next to the existing `C_KEYWORDS` /
   `CPP_KEYWORDS` / `RUST_KEYWORDS`. (If the lexer uses
   multiple keyword classes — Lua / SQL / HTML — add
   `<LANG>_KEYWORDS2` / `_KEYWORDS3` for the secondary
   classes.) These stay in `core::lang` so a future tool or
   plugin can read them without depending on `ui_win32`.
3. **Style mapping.** In `crates/ui_win32/src/lib.rs`,
   underneath the existing `CPP_STYLES` / `RUST_STYLES` blocks,
   author `<LANG>_STYLES: &[(usize, StyleSlot)]` listing every
   `SCE_*_INDEX` the lexer emits paired with its palette slot
   (`Comment` / `Keyword` / `String` / `Number` / …).
   Cross-reference the lexer's source in
   `vendor/lexilla/lexers/Lex<Lang>.cxx` so no SCE_* index is
   accidentally skipped. New `StyleSlot` variants are added to
   the enum + `slot_color` if a slot the existing palette
   doesn't cover is genuinely needed (Type? Function? Tag?) —
   reuse over invention, but add when warranted.
4. **Font modifiers.** Author `<LANG>_ITALIC` / `<LANG>_BOLD:
   &[usize]` lists for the SCE_* indices that want those
   modifiers (typically `SCE_*_COMMENT*` → italic and
   `SCE_*_WORD` → bold).
5. **Theme const.** Build `<LANG>_THEME: LangTheme { ... }`
   wiring all four pieces.
6. **Dispatch.** Add an `else if lang == L_<LANG> { Some(&<LANG>_THEME) }`
   arm to `lang_theme()`. For LexCPP-family languages (Java,
   JS, TS, Go, C#, Obj-C, Swift, RC) the per-language theme
   reuses `CPP_STYLES` / `CPP_ITALIC` / `CPP_BOLD` — only the
   keyword list differs.
7. **Coverage row.** Update this matrix's row from 🟡 to ✅,
   bump the total at the top.
8. **Verify.** Open a sample file, pick the language from the
   Language menu, and confirm comments / strings / numbers /
   keywords pick up visibly distinct colours. (No automated
   test gates this — `lang_theme_tests` covers framework shape
   but visual correctness is a manual demo step. The Phase 4
   demo gate already requires opening a `.cpp` and `.rs` to
   confirm highlighting; Phase 4.5 extends that to every ✅
   row.)

The framework itself has its own unit tests
(`lang_theme_tests` in `ui_win32`) verifying that wired
languages return a `Some(&theme)` with a non-empty keyword
list and a substantive style mapping, that LexCPP-family
languages share their style table by reference, and that
unwired languages correctly return `None`. Adding a row
extends these tests as appropriate.

## Coverage as of 2026-05-13

Phase 4.5 framework landed in an earlier commit; the table-driven
`lang_theme()` dispatch in `ui_win32` is wired. C, C++, and Rust
were migrated onto it as the no-op verification; PHP is the first
language added on top of the new framework. The framework's unit
tests (in the `lang_theme_tests` module) pin the contract going
forward.

PHP brings in a shared `HYPERTEXT_STYLES` table covering both
`SCE_H_*` (HTML wrapper) and `SCE_HPHP_*` (PHP code inside
`<?php ... ?>`). The hypertext lexer is shared across PHP / HTML
/ ASP / JSP — once those rows are wired, each will reuse
`HYPERTEXT_STYLES` and only differ in their per-language keyword
list. This mirrors the `CPP_STYLES` pattern across LexCPP family.

Subsequent commits add rows row-by-row. The matrix's
percentage updates per ✅ promotion.

Total: 89 rows. ✅ 7 / 🟡 81 / ⚫ 1.

**C# (2026-05-13):** rides the shared `CPP_STYLES` / `CPP_ITALIC` /
`CPP_BOLD` table from the LexCPP family — only the keyword list
differs from C / C++. `CS_KEYWORDS` (class 0, blue) carries C# 12
reserved words, contextual keywords (`async` / `await` / `partial`
/ `record` / `init` / `required` / `scoped` / `file` / `global` /
`with` / `and` / `or` / `not` / `when` / ...), and LINQ query
vocabulary (`from` / `where` / `select` / `group` / `into` /
`orderby` / `join` / `let` / `on` / `equals` / `by` / `ascending`
/ `descending`). Authored by a 7-agent research-and-adversarial-verify
workflow; preprocessor directives, `args`, `extension`, and
`field` deliberately omitted (rationale in `CS_KEYWORDS` docstring).

**Objective-C (2026-05-14):** rides the same shared `CPP_STYLES` /
`CPP_ITALIC` / `CPP_BOLD` table — Objective-C is a strict C
superset, so the LexCPP style indices map identically. Class 0
(`OBJC_KEYWORDS`) covers ObjC directives without the leading `@`
(`interface`, `implementation`, `end`, `protocol`, `property`,
`synthesize`, `try`, `catch`, `throw`, `autoreleasepool`,
`synchronized`, etc. — LexCPP tokenises `@` as an operator
separately, so the bare identifier is what the wordlist matches),
ARC ownership and bridge-cast qualifiers (`__weak` / `__strong` /
`__bridge` family / `__autoreleasing` / `__unsafe_unretained`),
the `__block` capture annotation, Distributed Objects method
qualifiers (`in` / `out` / `inout` / `oneway` / `bycopy` /
`byref`), constants (`YES` / `NO` / `nil` / `Nil` / `NULL` /
`true` / `false`), and `self` / `super`. Class 1
(`OBJC_KEYWORDS_2`) carries ObjC type vocabulary (`id` / `Class` /
`SEL` / `IMP` / `BOOL` / `instancetype` / `Method` / `Ivar` /
`Protocol`), Clang nullability qualifiers (`_Nullable` /
`_Nonnull` / `_Null_unspecified`), lightweight-generics variance
qualifiers (`__kindof` / `__covariant` / `__contravariant`), and
the full C primitive set. Authored by a 7-agent
research-and-adversarial-verify workflow; library typedefs
(`NSInteger` / `NSString` / `CGFloat` / ...) and Apple framework
class names deliberately omitted.

**Java (2026-05-14):** rides the same shared `CPP_STYLES` /
`CPP_ITALIC` / `CPP_BOLD` table from the LexCPP family. Class 0
(`JAVA_KEYWORDS`, 58 entries) covers JLS §3.9 reserved words
(41, including the never-implemented `const` / `goto`), modern
contextual keywords (`yield` / `record` / `sealed` / `permits` /
`when`), the full Java 9+ module-system directive set (`module`
/ `exports` / `requires` / `opens` / `uses` / `provides` / `to` /
`with` / `transitive`), and the literal constants (`true` /
`false` / `null`). Class 1 (`JAVA_KEYWORDS_2`, 10 entries) covers
the 8 primitives plus `void` and `var` (Java 10 type-inference
contextual keyword, classed with types per the C# precedent).
Authored by a 7-agent research-and-adversarial-verify workflow;
`non-sealed` deliberately excluded (hyphen breaks Lexilla's
identifier tokenisation, same trade-off Notepad++ accepts).

**LexCPP-family WORD2 split (2026-05-13 follow-up):** C / C++ / C#
/ Objective-C / Java all install **two** keyword classes — class 0
for control-flow / modifier reserved words (blue, `SCE_C_WORD`),
class 1 for primitive type aliases (steel blue, `SCE_C_WORD2`). Matches
Notepad++'s default blue-vs-steel-blue rendering. Class-1 consts:
`C_KEYWORDS_2` (`char` / `double` / `float` / `int` / `long` /
`short` / `signed` / `unsigned` / `void` plus the `_Bool` /
`_Complex` / `_Imaginary` C99 set), `CPP_KEYWORDS_2` (adds `bool`
/ `wchar_t` / `char8_t` / `char16_t` / `char32_t`), `CS_KEYWORDS_2`
(`bool` / `byte` / `char` / `decimal` / `double` / `dynamic` /
`float` / `int` / `long` / `nint` / `nuint` / `object` / `sbyte` /
`short` / `string` / `uint` / `ulong` / `ushort` / `var` / `void`).
Future JS / TS / Go / Swift / RC rows follow the
same two-class shape.

**Follow-up landed 2026-05-13:** every `Lex*.cxx` already in
`crates/scintilla-sys/build.rs`'s compile list is now registered
in the lexer catalog (`LexillaShim.cxx`). Prior to this, only
`lmCPP` / `lmHTML` / `lmNull` / `lmPHPSCRIPT` / `lmRust` / `lmXML`
were catalog entries — the remaining ~70 `Lex*.cxx` were
compiled into the binary but `CreateLexer(name)` returned
nullptr for them at runtime. Wiring any 🟡 row going forward is
now purely a host-theme change (keyword list + style table); no
further shim work needed.

## Languages

| Language | LangType id | Lexer | Keywords | Theme | Status |
| --- | --- | --- | --- | --- | --- |
| Normal Text | 0 | — | — | — | ⚫ |
| Ada | 42 | `ada` | ⚫ | ⚫ | 🟡 |
| ASN.1 | 65 | `asn1` | ⚫ | ⚫ | 🟡 |
| ASP | 16 | `hypertext` | ⚫ | ⚫ | 🟡 |
| Assembly | 32 | `asm` | ⚫ | ⚫ | 🟡 |
| AutoIt | 40 | `au3` | ⚫ | ⚫ | 🟡 |
| AviSynth | 66 | `avs` | ⚫ | ⚫ | 🟡 |
| BaanC | 60 | `baan` | ⚫ | ⚫ | 🟡 |
| Batch | 12 | `batch` | ⚫ | ⚫ | 🟡 |
| Blitzbasic | 67 | `blitzbasic` | ⚫ | ⚫ | 🟡 |
| C | 2 | `cpp` | ✅ | ✅ | ✅ |
| C# | 4 | `cpp` | ✅ | ✅ | ✅ |
| C++ | 3 | `cpp` | ✅ | ✅ | ✅ |
| Caml | 41 | `caml` | ⚫ | ⚫ | 🟡 |
| CMake | 48 | `cmake` | ⚫ | ⚫ | 🟡 |
| COBOL | 50 | `COBOL` | ⚫ | ⚫ | 🟡 |
| CoffeeScript | 56 | `coffeescript` | ⚫ | ⚫ | 🟡 |
| CSound | 70 | `csound` | ⚫ | ⚫ | 🟡 |
| CSS | 20 | `css` | ⚫ | ⚫ | 🟡 |
| D | 52 | `d` | ⚫ | ⚫ | 🟡 |
| Diff | 33 | `diff` | ⚫ | ⚫ | 🟡 |
| Erlang | 71 | `erlang` | ⚫ | ⚫ | 🟡 |
| ErrorList | 92 | `errorlist` | ⚫ | ⚫ | 🟡 |
| ESCRIPT | 72 | `escript` | ⚫ | ⚫ | 🟡 |
| Forth | 73 | `forth` | ⚫ | ⚫ | 🟡 |
| Fortran (fixed form) | 59 | `f77` | ⚫ | ⚫ | 🟡 |
| Fortran (free form) | 25 | `fortran` | ⚫ | ⚫ | 🟡 |
| Freebasic | 69 | `freebasic` | ⚫ | ⚫ | 🟡 |
| GDScript | 86 | `gdscript` | ⚫ | ⚫ | 🟡 |
| Go | 88 | `cpp` | ⚫ | ⚫ | 🟡 |
| Gui4Cli | 51 | `gui4cli` | ⚫ | ⚫ | 🟡 |
| Haskell | 45 | `haskell` | ⚫ | ⚫ | 🟡 |
| Hollywood | 87 | `hollywood` | ⚫ | ⚫ | 🟡 |
| HTML | 8 | `hypertext` | ⚫ | ⚫ | 🟡 |
| INI file | 13 | `props` | ⚫ | ⚫ | 🟡 |
| Inno Setup | 46 | `inno` | ⚫ | ⚫ | 🟡 |
| Intel HEX | 62 | `ihex` | ⚫ | ⚫ | 🟡 |
| Java | 6 | `cpp` | ✅ | ✅ | ✅ |
| Javascript | 58 | `cpp` | ⚫ | ⚫ | 🟡 |
| JSON | 57 | `json` | ⚫ | ⚫ | 🟡 |
| JSON5 | 94 | `json` | ⚫ | ⚫ | 🟡 |
| JSP | 55 | `hypertext` | ⚫ | ⚫ | 🟡 |
| KIXtart | 39 | `kix` | ⚫ | ⚫ | 🟡 |
| LaTeX | 74 | `latex` | ⚫ | ⚫ | 🟡 |
| Lisp | 30 | `lisp` | ⚫ | ⚫ | 🟡 |
| Lua | 23 | `lua` | ⚫ | ⚫ | 🟡 |
| Makefile | 10 | `makefile` | ⚫ | ⚫ | 🟡 |
| Matlab | 44 | `matlab` | ⚫ | ⚫ | 🟡 |
| MMIXAL | 75 | `mmixal` | ⚫ | ⚫ | 🟡 |
| Nim | 76 | `nim` | ⚫ | ⚫ | 🟡 |
| Nncrontab | 77 | `nncrontab` | ⚫ | ⚫ | 🟡 |
| NSIS | 28 | `nsis` | ⚫ | ⚫ | 🟡 |
| Objective-C | 5 | `cpp` | ✅ | ✅ | ✅ |
| OScript | 78 | `oscript` | ⚫ | ⚫ | 🟡 |
| Pascal | 11 | `pascal` | ⚫ | ⚫ | 🟡 |
| Perl | 21 | `perl` | ⚫ | ⚫ | 🟡 |
| PHP | 1 | `hypertext` | ✅ | ✅ | ✅ |
| PostScript | 35 | `ps` | ⚫ | ⚫ | 🟡 |
| PowerShell | 53 | `powershell` | ⚫ | ⚫ | 🟡 |
| Properties | 34 | `props` | ⚫ | ⚫ | 🟡 |
| Purebasic | 68 | `purebasic` | ⚫ | ⚫ | 🟡 |
| Python | 22 | `python` | ⚫ | ⚫ | 🟡 |
| R | 54 | `r` | ⚫ | ⚫ | 🟡 |
| Raku | 89 | `raku` | ⚫ | ⚫ | 🟡 |
| REBOL | 79 | `rebol` | ⚫ | ⚫ | 🟡 |
| Registry | 80 | `registry` | ⚫ | ⚫ | 🟡 |
| Resource file | 7 | `cpp` | ⚫ | ⚫ | 🟡 |
| Ruby | 36 | `ruby` | ⚫ | ⚫ | 🟡 |
| Rust | 81 | `rust` | ✅ | ✅ | ✅ |
| S-Record | 61 | `srec` | ⚫ | ⚫ | 🟡 |
| SAS | 91 | `sas` | ⚫ | ⚫ | 🟡 |
| Scheme | 31 | `lisp` | ⚫ | ⚫ | 🟡 |
| Shell | 26 | `bash` | ⚫ | ⚫ | 🟡 |
| Smalltalk | 37 | `smalltalk` | ⚫ | ⚫ | 🟡 |
| Spice | 82 | `spice` | ⚫ | ⚫ | 🟡 |
| SQL | 17 | `sql` | ⚫ | ⚫ | 🟡 |
| Swift | 64 | `cpp` | ⚫ | ⚫ | 🟡 |
| TCL | 29 | `tcl` | ⚫ | ⚫ | 🟡 |
| Tektronix extended HEX | 63 | `tehex` | ⚫ | ⚫ | 🟡 |
| TeX | 24 | `tex` | ⚫ | ⚫ | 🟡 |
| TOML | 90 | `toml` | ⚫ | ⚫ | 🟡 |
| txt2tags | 83 | `txt2tags` | ⚫ | ⚫ | 🟡 |
| TypeScript | 85 | `cpp` | ⚫ | ⚫ | 🟡 |
| Verilog | 43 | `verilog` | ⚫ | ⚫ | 🟡 |
| VHDL | 38 | `vhdl` | ⚫ | ⚫ | 🟡 |
| Visual Basic | 18 | `vb` | ⚫ | ⚫ | 🟡 |
| Visual Prolog | 84 | `visualprolog` | ⚫ | ⚫ | 🟡 |
| XML | 9 | `xml` | ⚫ | ⚫ | 🟡 |
| YAML | 49 | `yaml` | ⚫ | ⚫ | 🟡 |

## Notes

- **Shared lexers.** Several `LANG_TABLE` rows route to the same
  Lexilla lexer name — `cpp` covers C / C++ / C# / Java /
  Javascript / Objective-C / Resource file / Swift / TypeScript /
  Go (10 entries); `hypertext` covers HTML / ASP / JSP / PHP (4
  entries); `lisp` covers Lisp / Scheme; `props` covers INI /
  Properties; `json` covers JSON / JSON5. The lexer stays the
  same; what differs is the **keyword list** the host installs.
  When wiring these up, the `StyleSlot` mapping table can be
  shared across the family — only the `&str` keyword list differs.
- **Future SCE_* batches.** When a new lexer is wired up, its
  `SCE_*` style constants land in `crates/scintilla-sys/src/lib.rs`
  alongside the existing `SCE_C_*` / `SCE_RUST_*` / `SCE_P_*` /
  `SCE_JSON_*` / `SCE_SH_*` / `SCE_LUA_*` / `SCE_SQL_*` /
  `SCE_YAML_*` / `SCE_TOML_*` / `SCE_CSS_*` blocks. The numeric
  values come from `vendor/lexilla/include/SciLexer.h` and must
  not be guessed.
- **Lexer name typos are silent failures.** `set_lexer_by_name`
  routes through Lexilla's `CreateLexer` which returns NULL for
  unknown names; the host falls through to `clear_lexer` +
  default styles. The `LANG_TABLE` row's `lexer:` field is the
  source of truth — copy from there, don't retype.
- **Performance.** Each ✅ row's theme application runs ~10–25
  `style_set_fore` calls plus a `SCI_COLOURISE`. Together that's
  well inside Phase 4.5's keystroke budget — measured
  empirically on C++/Rust today, which both already do this work
  on every tab switch. Adding more rows doesn't change the
  per-switch cost (only the active row's mappings run).
