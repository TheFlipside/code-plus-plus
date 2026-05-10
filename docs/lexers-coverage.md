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
| 🟡 | Lexer attached and tokenising; no host keyword list and no host theme. Buffer renders uniformly black-on-white because every `SCE_*_*` style resolves to `STYLE_DEFAULT` after `SCI_STYLECLEARALL`. |
| ⚫ | No Lexilla lexer (`LANG_TABLE` row has `lexer: None`). Either by design (`L_TEXT` — plain text never highlights) or because no Lexilla lexer matches the language. Effectively a permanent state for the named row. |
| ⏸ | Reserved for future host-side opt-out (e.g. a lexer the host deliberately leaves off the menu pending review). None today. |

## How to mark a row ✅

The Phase 4.5 framework in `Win32Ui::apply_lang` reads a
`LangTheme` table entry per language. Adding one row means:

1. Confirm the language's `SCE_*_*` style constants are
   declared in `crates/scintilla-sys/src/lib.rs`. The starter
   set (Python, JSON, Bash, Lua, SQL, YAML, TOML, CSS) is
   already there as scaffolding; new lexers add their batch
   of constants with a comment citing the upstream
   `vendor/lexilla/include/SciLexer.h` line range.
2. Author a `&str` keyword list (or several, for lexers with
   multiple keyword classes — primary keywords, types,
   built-ins, etc.).
3. Author a `&[(SCE_*_INDEX, StyleSlot)]` table that maps
   each style index the lexer emits onto a slot in the shared
   palette (Comment / String / Number / Keyword / Operator /
   …).
4. Add the row to `lang_theme()`'s match.
5. Verify by opening a sample file, picking the language, and
   confirming colours are visible.
6. Update the row below from 🟡 to ✅.

## Coverage as of 2026-05-10

Three rows ✅ today (C, C++, Rust — wired in the original
Phase 4 m1 inline branches). Every other row with a Lexilla
lexer is 🟡 pending Phase 4.5. `Normal Text` is ⚫ by design.

Total: 89 rows. ✅ 3 / 🟡 85 / ⚫ 1.

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
| C# | 4 | `cpp` | ⚫ | ⚫ | 🟡 |
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
| Java | 6 | `cpp` | ⚫ | ⚫ | 🟡 |
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
| Objective-C | 5 | `cpp` | ⚫ | ⚫ | 🟡 |
| OScript | 78 | `oscript` | ⚫ | ⚫ | 🟡 |
| Pascal | 11 | `pascal` | ⚫ | ⚫ | 🟡 |
| Perl | 21 | `perl` | ⚫ | ⚫ | 🟡 |
| PHP | 1 | `hypertext` | ⚫ | ⚫ | 🟡 |
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
