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
| — (Keywords column only) | Not applicable. The lexer takes no wordlists at all — host installs none by design. Currently used for `props` (INI / Properties), a pure line-prefix classifier. A row with `—` in the Keywords column and ✅ in the Theme column is still ✅ overall: the wiring is complete, there are simply no keywords to wire. |
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

Total: 89 rows. ✅ 32 / 🟡 56 / ⚫ 1.

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

**Pascal (2026-05-14):** uses Lexilla's `pascal` lexer
(`LexPascal.cxx`). Substantial 13-mapping `PASCAL_STYLES` table
covering three syntactic comment forms (`{...}` / `(*...*)` /
`//`), two preprocessor dialects (`{$...}` and `(*$...*)`
Delphi/FPC directives), decimal + `$`-prefixed hex numbers,
words, single-quoted strings + `#nn` character literals +
Delphi-11+ triple-quoted multiline strings, operators, and
`SCE_PAS_ASM` → `Keyword2` for inline-assembler block content
(distinct steel-blue treatment, matches Notepad++'s Pascal
scheme). `PASCAL_KEYWORDS` (172 entries, all-lowercase) covers
the union of ISO Pascal + Delphi (Object Pascal) + Free Pascal
dialects.

**Critical: lexer lowercases the source.** `LexPascal.cxx:278`
calls `sc.GetCurrentLowered` before wordlist lookup, so the
wordlist MUST be all-lowercase. Pascal source code can use any
casing (`Begin` / `BEGIN` / `begin` all match `begin`) — the
case-insensitive convention is honoured transparently.

Authored by a 7-agent workflow. The correctness verifier
flagged `break` / `continue` / `exit` / `halt` / `new` /
`dispose` as blockers (technically System-unit intrinsic
procedures, not reserved words). Compromise applied: kept
`break` / `continue` / `exit` (universal editor convention for
control-flow words; matches Notepad++ and upstream Lexilla
default), dropped `halt` / `new` / `dispose` (closer to pure
procedure calls). Override documented in `PASCAL_KEYWORDS`
docstring.

Context-sensitive accessors (`index` / `name` / `read` /
`write` / `default` / `nodefault` / `stored` / `implements` /
`readonly` / `writeonly` / `add` / `remove`) included —
`LexPascal.cxx:296-306` handles property/exports-block
suppression internally. Predefined types (`integer` /
`boolean` / `string` / `byte` / `word` / `cardinal` /
`ansistring` / etc.) included for editor-baseline consistency.

`DEFAULT` (0), `IDENTIFIER` (1), `STRINGEOL` (11)
intentionally unmapped — DEFAULT / IDENTIFIER mirror the
`SCE_C_*` omission pattern; STRINGEOL pending future
`StyleSlot::Error`. `pascal_uses_lexpascal_dedicated_theme`
test pins the 13-mapping shape + class-0-only structure +
canonical wiring + non-reuse of `CPP_STYLES`.

**Batch (2026-06-10):** uses Lexilla's `batch` lexer
(`LexBatch.cxx`) — a small case-insensitive lexer with a compact
7-mapping `BATCH_STYLES` table covering line comments (`REM` /
`::`), two distinct keyword classes (cmd.exe intrinsics in
wordlist 0 → `SCE_BAT_WORD` → Keyword bold blue, PATH-discovered
external programs in wordlist 1 → `SCE_BAT_COMMAND` → Keyword2
steel blue), `:label` markers + leading `@` echo-suppress
directives (both → Preprocessor, the "out-of-band syntax marker"
slot since `StyleSlot` has no `Label` variant), operators
(`SCE_BAT_OPERATOR`), and `AFTER_LABEL` trailing text →
Comment (LexBatch's own `lexicalClasses[]` describes class 8 as
comment-class). The Keyword / Keyword2 split mirrors cmd.exe's
own dispatch model — class 0 = "cmd parsed this", class 1 =
"cmd spawned this".

**Critical: lexer lowercases the source.** `LexBatch.cxx:233`
calls `MakeLowerCase(styler[i])` before wordlist lookup, so both
wordlists MUST be all-lowercase. Batch source can use any casing
(`IF` / `If` / `if` all match) — case-insensitivity honoured
transparently. The `batch_uses_lexbatch_two_class_theme` test
pins the all-lowercase invariant structurally so a future
uppercase entry trips CI rather than silently failing to match.

`BATCH_KEYWORDS` (class 0, 73 entries) covers cmd.exe intrinsics:
control flow (`if` / `else` / `for` / `in` / `do` / `goto` /
`call` / `exit`), `IF` predicates + comparison operators
(`defined` / `not` / `errorlevel` / `exist` / `equ` / `neq` /
`lss` / `leq` / `gtr` / `geq`), core builtins (`set` / `setlocal`
/ `endlocal` / `shift` / `echo` / `rem` / `pause` / etc.),
filesystem builtins with alias spellings (`cd`/`chdir`,
`mkdir`/`md`, `rmdir`/`rd`, `del`/`erase`, `ren`/`rename`, plus
`mklink`), environment / info (`ver` / `vol` / `date` / `time` /
`path` / `color` / `assoc` / `ftype` / `label` / `help` /
`print`), control-flow-adjacent (`choice` / `start` / `break` /
`verify` / `loadhigh` / `lh`), `FOR /F` option keywords (`tokens`
/ `delims` / `eol` / `skip` / `usebackq`), `IF cmdextversion`,
and `SETLOCAL` mode toggles (`enabledelayedexpansion` /
`disabledelayedexpansion` / `enableextensions` /
`disableextensions`). `loadhigh` / `lh` included specifically
because `LexBatch.cxx:360` explicitly tests for them when
applying the "next token is an external command" rule.

`BATCH_KEYWORDS_2` (class 1, 87 entries) covers OS-shipped Win32
utilities the average batch corpus calls by bare name: file /
archive (`xcopy` / `robocopy` / `findstr` / `forfiles` / `fsutil`
/ `icacls` / `takeown` / etc.), codepage + clipboard (`chcp` /
`clip` / `mode`), system info (`systeminfo` / `whoami` /
`tasklist` / `auditpol`), process control (`taskkill` / `runas` /
`sc` / `schtasks` / `wmic` / `shutdown` / `timeout`), scripting
hosts (`powershell` / `pwsh` / `cscript` / `wscript` / `mshta`),
installers / loaders (`msiexec` / `rundll32` / `regsvr32` /
`regedit` / `reg`), network (`ping` / `ipconfig` / `netsh` /
`tracert` / `route` / `arp` / `netstat` / `nslookup` / `telnet`
/ etc.), disk / format (`chkdsk` / `diskpart` / `format` /
`mountvol`), servicing / image (`dism` / `sfc` / `pnputil` /
`bcdedit` / `gpresult` / `gpupdate` / `bitsadmin` / `certutil`),
event log (`eventcreate` / `wevtutil`), and time (`w32tm`).

Authored by a 7-agent research-and-adversarial-verify workflow.
The correctness verifier caught three stale tokens from the
draft (`devmgmt` — an `.msc` snap-in, not an executable;
`eventquery` — a removed `.vbs`; `eventtriggers` — removed
binary) and the missing `loadhigh` / `lh` pair. The completeness
verifier surfaced ~20 high-frequency external tools missing from
the draft (`chcp` / `timeout` / `certutil` / `msiexec` /
`rundll32` / `cscript` / `wscript` / etc.) which were all added.
The format verifier confirmed the all-lowercase + first-hit-no-
overlap invariants hold.

**Deliberate exclusions:** dev-toolchain binaries (`msbuild` /
`cl` / `link` / `nmake` / `mingw32-make`) — not OS-shipped, ride
along with Visual Studio / MinGW installs and only resolve
inside Developer Command Prompts; styling them as known commands
implies endorsing a specific toolchain. Unix tools (`less` /
`ifconfig`) — not on Windows; the Windows equivalents (`more`
internal, `ipconfig` external) are covered. Switch tokens (`/a`
/ `/p` / `/f` / `/?` / etc.) — flags, not keywords. Pseudo-
variables (`%errorlevel%` / `%cd%` / etc.) — render through
`%VAR%` expansion under `SCE_BAT_IDENTIFIER`. Device names
(`nul` / `con` / `prn`) — cmd doesn't lex them as keywords at
command position. `DEFAULT` (0) and `IDENTIFIER` (6) style
indices intentionally unmapped — mirror `SCE_C_*` omission
pattern (generic identifiers carry no language meaning beyond
default foreground). Parentheses `( )` get `SCE_BAT_DEFAULT`
from the lexer itself (`LexBatch.cxx:595`), NOT `SCE_BAT_OPERATOR`
— don't be fooled by the LexicalStyles class description.

Not added to `wired_languages_have_complete_themes` (its
7-mapping table is below the 8-floor calibrated for LexCPP /
hypertext families — legitimate, LexBatch simply has fewer
emission categories). Dedicated `batch_uses_lexbatch_two_class_theme`
test pins the canonical wiring instead, plus an explicit no-
overlap check between the two wordlists.

**INI file + Properties (2026-06-10):** both menu rows use Lexilla's
`props` lexer (`LexProps.cxx`) and share a single `PROPS_THEME`.
`L_INI` (`.ini`) and `L_PROPS` (`.properties`) exist as separate
menu entries because Notepad++ surfaces them that way, but the
underlying lexer behaviour is identical so they route to the same
theme. The `ini_and_props_share_props_theme_with_no_wordlists`
test pins this with a `std::ptr::eq` assertion — stronger than
value-equality, it catches any future divergence into two
silently-identical copies.

`LexProps` is the framework's smallest lexer: a pure line-prefix
classifier with **NO wordlists**. `ColourisePropsDoc`'s
`WordList *[]` parameter is unused; classification is purely
line-prefix-based (`#` / `!` / `;` → COMMENT, `[` → SECTION,
`@` → DEFVAL, otherwise scan for `=` or `:` to split KEY from
the value tail). `core::lang` therefore has no new keyword
consts in this commit — `PROPS_THEME` installs no
`SCI_SETKEYWORDS` calls, pinned structurally by
`assert!(ini.keywords.is_empty(), ...)`.

`PROPS_STYLES` is a compact 5-mapping table — does NOT reuse
`CPP_STYLES`, `HYPERTEXT_STYLES`, `MAKEFILE_STYLES`,
`PASCAL_STYLES`, or `BATCH_STYLES`. Mappings: COMMENT (1) →
Comment italic, SECTION (2) → Keyword bold blue (`[section]`
headers are the structural anchors a reader scans for, same
role `SCE_MAKE_TARGET` plays for Makefile targets),
ASSIGNMENT (3) → Operator (the `=` / `:` separator),
DEFVAL (4) → Preprocessor (`@`-prefixed Java `.properties`
default-value syntax is an out-of-band marker, same
"directive" slot Batch uses for its leading `@` echo-
suppress), KEY (5) → Keyword2 steel blue (key names are
named identifiers on the left, distinct from SECTION's
structural treatment). DEFAULT (0) intentionally unmapped —
value text (post-`=`) is the dominant occupant of this slot
and stays at default foreground, since INI values are
arbitrary user data with no canonical meaning to colour.

This is the first ✅ row in the matrix with `—` in the
Keywords column instead of ✅ — the legend has a new entry
documenting the convention. Two rows flip ✅ per commit
because `L_INI` and `L_PROPS` share `PROPS_THEME` exactly.

Not added to `wired_languages_have_complete_themes`
(5-mapping table is below the 8-floor; `LexProps` simply has
fewer emission categories). Dedicated
`ini_and_props_share_props_theme_with_no_wordlists` test pins
the canonical wiring instead, including the zero-wordlist
invariant.

**ASP (2026-06-10):** Classic ASP rides the same hypertext lexer
as HTML / PHP / XML — same `lmHTML` ("hypertext") factory, just
with the `lexer.html.allow.asp` property defaulting to true so
`<% %>` block parsing fires. `ASP_THEME` installs THREE wordlist
classes per LexHTML's `htmlWordListDesc[]`: class 0 = HTML tags
(reuses canonical `HTML_KEYWORDS`), class 1 = JavaScript reserved
words (`JAVASCRIPT_KEYWORDS`, 49 entries), class 2 = VBScript
reserved words (`VBSCRIPT_KEYWORDS`, 133 entries, all-lowercase).

**Headline infrastructure win: `HYPERTEXT_STYLES` gains four new
embedded-script ranges in the same commit** — `SCE_HJ_*`
(client-side JS, indices 40-53), `SCE_HJA_*` (ASP-server-side
JS, 55-68), `SCE_HB_*` (client-side VBScript, 70-77), `SCE_HBA_*`
(ASP-server-side VBScript, 80-87). The extension is wired once
into the shared table so every hypertext-family theme benefits:
HTML / PHP files with `<script>` blocks now style comments,
strings, numbers, and operators correctly inside the script
tags. Keyword highlighting on those blocks is the only piece
that still requires per-theme follow-up (HTML / PHP themes don't
yet install class 1 / class 2; tracked as a one-line follow-up
on the HTML and PHP rows). Same future infrastructure also
covers JSP and the future `L_JAVASCRIPT` row.

**VBScript-specific lexer quirks** documented in the new SCE
constant block in `scintilla-sys`:

- VBScript has only `_COMMENTLINE` (single-line via `'` or
  `Rem`, no block-comment form) where JavaScript has three
  comment classes (`_COMMENT` / `_COMMENTLINE` / `_COMMENTDOC`).
  Both `SCE_HB_COMMENTLINE` (72) and `SCE_HBA_COMMENTLINE` (82)
  retain the upstream naming — getting that name wrong is a
  build-breaking bug the synthesis stage of the research
  workflow caught.
- VBScript has only one `_STRING` class (no single-quoted
  strings — `'` starts a comment).
- VBScript has its own `_IDENTIFIER` class (76 / 86) that JS
  lacks; intentionally unmapped per the `SCE_C_IDENTIFIER`
  omission pattern.

**`rem` is required in `VBSCRIPT_KEYWORDS`, not defensive.**
`LexHTML`'s `classifyWordHTVB` explicitly tests for `rem`
inside the VB classifier and only fires the
`SCE_HB_COMMENTLINE` styling if the wordlist lookup succeeds.
Removing `rem` would render `Rem this is a comment` as an
identifier followed by default-styled body text. The wordlist
docstring documents this requirement so a future "cleanup"
commit doesn't strip it.

**Compound forms tokenise as separate words.** `End If`,
`Loop While`, `Exit For`, `On Error Resume Next`, `Option
Explicit` — every constituent word is looked up individually
by the lexer and must appear in `VBSCRIPT_KEYWORDS`. The lexer
renders adjacent keyword-styled tokens; no special handling
needed.

**STRINGEOL indices (51 / 66 / 77 / 87) intentionally
unmapped** — pending the future `StyleSlot::Error` palette
addition. Mapping them to `String` would visually present
malformed input as intentional syntax. This brings the
codebase's deferred `Error`-slot migration list to 8 entries
(SGML_ERROR, SGML_1ST_PARAM_COMMENT, MAKE_IDEOL, PAS_STRINGEOL,
plus the four embedded-script STRINGEOLs added here).

**Deliberate scope exclusions:**

- **VB.NET-only tokens** (`module` / `namespace` / `imports` /
  `inherits` / `mybase` / `mustinherit` / `notinheritable` /
  `overrides` / `shadows` / `shared` / `withevents` / `handles`
  / `try` / `catch` / `finally` / `throw` / `continue` /
  `andalso` / `orelse` / `gettype` / ...) — don't exist in
  VBScript-under-WSH. The `L_ASP` row scopes to `.asp` (Classic
  ASP) only; `.aspx` (ASP.NET) is a separate language not
  covered here. Including them would mis-colour a user identifier
  of the same name.
- **ASP intrinsic objects** (`Request` / `Response` / `Server` /
  `Session` / `Application` / `ObjectContext`) — host-provided
  ActiveX objects supplied by IIS, not VBScript language
  constructs. Notepad++'s default doesn't list them either.
  Users who want them highlighted can extend via the substyle
  mechanism `LexHTML` exposes (`SCE_HB_WORD` is in
  `styleSubable[]`); UI for substyle configuration is a
  pre-Phase-5 polish item.
- **JS global objects / DOM APIs** (`console` / `window` /
  `document` / `Math` / `Object` / `Array` / jQuery `$` / ...)
  — identifiers bound at runtime, not keywords.
- **Class 3 (Python / PythonASP)** — defer until `L_PYTHON`
  needs the `SCE_HP_*` range wired.

Authored by a 7-agent research-and-adversarial-verify workflow.
The correctness verifier caught the build-breaking
`SCE_HB_COMMENT` / `SCE_HBA_COMMENT` typo (upstream defines
those indices as `*_COMMENTLINE` — VBScript has no block comment
class). The format verifier caught synthesis token-count
inflations (claimed 53 JS / 160 VB; actual 49 / 133). The
completeness verifier flagged VB.NET tokens missing — scope
decision documented above (Classic ASP only; ASP.NET is its own
language). The spurious `continue` token (VB.NET 8+ only) was
dropped from the VBScript list during synthesis review.

`asp_theme_installs_html_js_vbscript_classes` test pins the
canonical 3-class shape (HTML/JS/VBScript), reuse of the shared
`HYPERTEXT_*` tables, the structural "no class 3/4/5" guard, the
HTML wordlist-share with `HTML_THEME`, and the all-lowercase
invariant on both class 1 and class 2 (LexHTML lowercases VB
source before lookup, and ECMAScript convention has all
reserved words lowercase anyway).

**SQL (2026-06-10):** uses Lexilla's `sql` lexer (`LexSQL.cxx`).
Dedicated 14-mapping `SQL_STYLES` table — does NOT reuse
`CPP_STYLES`, `HYPERTEXT_STYLES`, `MAKEFILE_STYLES`,
`PASCAL_STYLES`, `BATCH_STYLES`, or `PROPS_STYLES`. Covers five
comment forms (block / line / doc / line-doc / SQL*Plus REM all
→ Comment), numbers, strings + characters, primary keywords
(class 0 → `SCE_SQL_WORD` → Keyword bold blue), types + builtin
functions (class 1 → `SCE_SQL_WORD2` → Keyword2 steel blue),
operators, SQL*Plus client-tool commands + PROMPT (→
Preprocessor — "out-of-band syntax marker" semantic, same
precedent as Batch's `@` echo-suppress directive), and PLDoc
`@tag` markers inside `/** */` doc comments (→ Keyword2).

**Critical: case-insensitive lexer.** `LexSQL.cxx:786` calls
`MakeLowerCase(styler[i+j])` on every candidate token before
keyword comparison. `SQL_KEYWORDS` (389 entries) and
`SQL_KEYWORDS_2` (215 entries) are stored all-lowercase. SQL
source can use any casing (`SELECT` / `Select` / `select` all
match) — the case-insensitive convention is honoured
transparently. Uppercase wordlist entries would never match.
The `sql_uses_lexsql_dedicated_theme_with_two_classes` test
pins this invariant structurally.

**Class split mirrors Notepad++'s shipped `langs.model.xml`.**
A token appears in exactly one wordlist (the test pins the
no-overlap invariant via a `HashSet` intersection check, same
shape as the Batch test). Class 0 covers statement-level
reserved words — DML / DDL / DCL verbs, clause keywords,
control flow, set ops, literals, and procedural vocabulary
from T-SQL / PL/SQL / PL/pgSQL. Class 1 covers built-in type
names (`int` / `varchar` / `timestamp` / `jsonb` / `uuid`) and
built-in functions (`count` / `coalesce` / `cast` / `extract`)
including the full window-function family (`row_number` /
`rank` / `dense_rank` / `ntile` / `lag` / `lead` / `first_value`
/ `last_value` / `nth_value` / `percent_rank` / `cume_dist`).
Window-frame keywords (`current` / `following` / `groups` /
`nulls` / `preceding` / `unbounded` / `window`) live in class 0
because they're structural clause vocabulary, not function
names.

**Dialect scope.** ANSI SQL:2016 baseline plus the four major
dialects — PostgreSQL, MySQL/MariaDB, Microsoft SQL Server
(T-SQL), and Oracle (PL/SQL). Hierarchical-query (`connect by`
/ `prior` / `level` / `rownum`), `merge`, `pivot`/`unpivot`,
`returning`, `ilike`, `lateral`, `forall` — all covered.
Dialect-specific types include PG (`cidr` / `inet` / `hstore` /
`tsvector` / `point`+geometric family), MySQL (`tinytext` /
`year` / `tinyblob`), SQL Server (`hierarchyid` /
`uniqueidentifier` / `sql_variant`), and Oracle (`number` /
`varchar2` / `rowid` / `urowid` / `bfile`).

**Classes 2-7 deliberately empty in v1.** The lexer exposes
eight wordlist classes via `sqlWordListDesc[]`: class 0
"Keywords", class 1 "Database Objects", class 2 "PLDoc", class
3 "SQL*Plus", and classes 4-7 "User Keywords 1-4". `SQL_THEME`
installs classes 0 and 1 today. PLDoc tag styling is niche
(Oracle PL/SQL-specific); SQL*Plus is Oracle CLI-specific;
user-customisable wordlists need a UI not yet built. Style
indices `SCE_SQL_SQLPLUS` (8) / `SCE_SQL_SQLPLUS_PROMPT` (9) /
`SCE_SQL_COMMENTDOCKEYWORD` (17) ARE mapped (to Preprocessor /
Preprocessor / Keyword2) so the lexer's structural syntactic
recognition (`@`-prefixed lines for SQL*Plus, `@tag` inside
doc comments for PLDoc) still fires visibly — only specific
command/tag names from wordlists go unrecognised. Adding the
class 2 / class 3 wordlists later is a one-line theme edit.

**Deliberate exclusions:**

- Cloud-warehouse extensions (Snowflake `qualify`,
  `match_recognize`, BigQuery `safe.`, Redshift / DuckDB
  dialect-specific vocabulary) — add per project demand.
- Vendor schema identifiers (`sys` / `information_schema` /
  `pg_catalog` / `dbo` / `master` / `mysql` /
  `performance_schema`) — identifiers, not keywords;
  including would mis-style legitimate user references.
- Optimiser hint contents (Oracle `/*+ ... */` body, T-SQL
  `OPTION (HASH JOIN)` inner words) — too dialect-specific
  and overlaps too aggressively with common identifiers.
- Hyphenated forms (`end-exec`) — `LexSQL.cxx`'s `iswordchar`
  treats `-` as an operator, so such entries can never match
  as a single token; they'd be silent dead weight.

Style indices intentionally unmapped: `DEFAULT` (0),
`IDENTIFIER` (11), `QUOTEDIDENTIFIER` (23), `QOPERATOR` (24)
all fall through to STYLE_DEFAULT (mirrors the `SCE_C_*`
omission pattern for generic identifiers / boundary text).
`COMMENTDOCKEYWORDERROR` (18) and `USER1..USER4` (19-22)
deferred to future `StyleSlot::Error` / per-user wordlist UI
respectively. This brings the codebase's deferred-Error-slot
migration list from 8 to 9 entries.

Authored by a 7-agent research-and-adversarial-verify
workflow. The correctness verifier caught two cross-class
duplicates (`path` and `repeat` in both class 0 and class 1
— resolved by routing both to class 1, where PG type / string
function fits are stronger), four fabricated tokens
(`lowercase` / `semicolon` / `subclass_origin` are not SQL
keywords; `end-exec` would never tokenise as one word with
`-` as operator), and synthesis count inflations (claimed
287 / 176; actual 389 / 215 after all fixes). The
completeness verifier caught the entire window-function
category missing from the initial draft (8 window-frame
keywords for class 0, 11 window functions + 11 statistical
aggregates + 7 T-SQL functions + 4 regex functions for
class 1) — all added before commit. The format verifier
confirmed the all-lowercase and no-overlap invariants hold
after the corrections, structurally pinned by the test
assertions.

`sql_uses_lexsql_dedicated_theme_with_two_classes` test
pins the 14-mapping shape, two-class structure, canonical
keyword constant links, all-lowercase invariant on both
wordlists, no-overlap invariant, and structural "no class
2-7" guard.

**Visual Basic (2026-06-10):** uses Lexilla's `vb` lexer
(`LexVB.cxx`). The `L_VB` row routes both `.vb` (VB.NET) and
`.vbs` (VBScript) extensions to the same lexer; the wordlists
cover the VB.NET superset (a strict superset of VBScript), so
`.vbs` files render correctly with no false negatives. VBA /
VB6 / VB Classic vocabulary is included transitively via
`defbool` / `ccur` / `clngptr` / `ptrsafe` / `lset` / `rset`
/ `load` / `unload` / `begin` / `attribute`.

Dedicated 10-mapping `VB_STYLES` table — does NOT reuse any
other framework style table (CPP / HYPERTEXT / MAKEFILE /
PASCAL / BATCH / PROPS / SQL — all 7 non-reuse assertions
structurally pinned in the test). Maps comments → Comment
italic, decimal/`&H`/`&O`/`&B` numbers AND `#1/1/2024#`
date literals → Number, `SCE_B_KEYWORD` (class 0, primary
reserved words) → Keyword bold blue,
`SCE_B_KEYWORD2`/`3`/`4` (classes 1/2/3) → Keyword2 steel-blue,
double-quoted strings → String, `#If` / `#Region` / `#Const`
preprocessor directives → Preprocessor bold (lexer-detected by
leading `#`, not by wordlist), operators → Operator. DEFAULT
(0), IDENTIFIER (7), STRINGEOL (9) intentionally unmapped —
fall through to STYLE_DEFAULT (mirrors `SCE_C_*` / `SCE_PAS_*`
omission pattern, plus STRINGEOL pending `StyleSlot::Error`).
This brings the deferred-Error-slot migration list from 9 to
10 entries.

**Critical: case-insensitive lexer.** `LexVB.cxx:208` calls
`sc.GetCurrentLowered(s, ...)` on every candidate token
before `keywords.InList(s)`. `VB_KEYWORDS` (199 entries) and
`VB_KEYWORDS_2` (53 entries) are stored all-lowercase. VB
source can use any casing (`If` / `IF` / `if` all match) —
the case-insensitive convention is honoured transparently.
Uppercase wordlist entries would never match. The
`vb_uses_lexvb_two_class_theme` test pins the all-lowercase
invariant structurally.

**Class split.** Class 0 (primary keywords) covers control
flow, declaration modifiers, class / module / namespace
syntax, error handling (`try` / `catch` / `finally` /
`throw`), type-cast operator keywords (`ctype` / `directcast`
/ `trycast` / `addressof` / `gettype` / `typeof` / `nameof`),
the `c<Type>` conversion-function family (Microsoft-reserved,
unlike the string / math / date intrinsics), sentinel
literals (`true` / `false` / `nothing` / `null`), logical /
comparison operator keywords (`and` / `andalso` / `or` /
`orelse` / `not` / `xor` / `mod` / `is` / `isnot` / `like` /
`eqv` / `imp` / `new`), `Option` directive vocabulary, LINQ
contextual keywords (`from` / `where` / `group` / `into` /
`join` / `equals` / `aggregate` / `distinct` / `order` /
`skip` / `take` / `ascending` / `descending`), async / iterator
contextual keywords (`async` / `await` / `yield` / `iterator`
/ `custom` / `when` / `using` / `synclock` / `with`), VBA
`Def<Type>` statements, VB6 form `Load` / `Unload`, the XML
literal namespace lookup `getxmlnamespace`. Class 1 (types +
intrinsics) covers the 16 VB.NET primitive types + VB Classic
/ VBScript / VBA dialect-extension types and literals
(`currency` / `variant` / `empty`) + 35 `vb<Name>` intrinsic
constants from `Microsoft.VisualBasic.Constants` (text /
line-ending: `vbcrlf` / `vbnewline` / `vbtab` etc.; MsgBox:
button groups / icons / default-button / modality / return
values).

**`rem` deliberately excluded from class 0.** `LexVB.cxx:212-
213` hard-codes `Rem` line-comment recognition before
consulting any wordlist; `Rem` lines style as `SCE_B_COMMENT`
regardless of whether `rem` is in class 0. The test pins this
structurally with `assert!(!class0.contains("rem"), ...)` so a
future "defensive" cleanup commit doesn't add it back as dead
weight.

**Independence from [`VBSCRIPT_KEYWORDS`].** The
`VBSCRIPT_KEYWORDS` const (added by the ASP commit) feeds the
**hypertext** lexer's class 2 for server-side VBScript inside
`<% %>` blocks — a different lexer surface deliberately
widened with library intrinsics (`msgbox` / `inputbox` / `chr`
/ etc.) for ASP. `VB_KEYWORDS` feeds `LexVB`'s class 0 and
follows Notepad++'s shipped `<Language name="vb">` `instre1`
convention of excluding those library identifiers — they are
not Microsoft-reserved keywords; including them would mis-colour
user identifiers of the same name. (Only the `c<Type>`
conversion family is included because Microsoft does list it
as reserved.)

**Deliberate exclusions** (Notepad++ ships some; trimmed as
dead vocabulary):

- Library intrinsics (`msgbox` / `inputbox` / `chr` / `asc` /
  `len` / `left` / `right` / `mid` function form / `trim` /
  `ucase` / `lcase` / `instr` / `replace` / `split` / `join`
  / `now` / `date` function form / `time` / `year` / etc.) —
  library identifiers, not Microsoft-reserved.
- .NET framework type names (`Form` / `Application` /
  `Console` / `System` / `Exception`) — library identifiers.
- ASP intrinsic objects (`request` / `response` / `server` /
  `session` / `application` / `objectcontext`) — same
  reasoning as in the ASP commit.
- `#`-prefixed preprocessor directives (`#if` / `#else` /
  `#region` / `#const` / `#externalsource` / `#disable` /
  `#enable`) — styled by lexer's `#`-prefix path, not by
  wordlist.
- `vb<Type>` `VarType` return-value constants (`vbInteger` /
  `vbLong` / `vbString` / etc.) — duplicate type-name spelling
  creates visual collision (`vbInteger` next to `Integer`
  both rendering as Keyword2).
- Colour constants (`vbBlack` / `vbBlue` / etc.), FileAttribute
  / TriState / CompareMethod / CallType / DateFirst* families
  — dead in modern .NET; modern code uses `Color.FromArgb` /
  `My.Computer.FileSystem` / etc.

Authored by a 7-agent research-and-adversarial-verify
workflow. The correctness verifier CONFIRMED with minor
nits (wordlist descriptors are `"Keywords"` / `"user1"` /
`"user2"` / `"user3"` upstream — the three `userN` slots
aren't user-customisable, they ARE the secondary keyword
classes; `rem` would be dead in the wordlist). The
completeness verifier flagged 5 missing tokens
(`ascending` / `descending` for LINQ sort direction, `off` /
`infer` for `Option` directive completeness, `getxmlnamespace`
for VB.NET XML literals) — all added before commit. The
format verifier CONFIRMED counts (199 / 53), no overlap,
no duplicates, all lowercase, ASCII, style indices match
SciLexer.h.

`vb_uses_lexvb_two_class_theme` test pins the 10-mapping
shape, two-class structure, canonical keyword links,
all-lowercase invariant, no-overlap invariant, no-class-2/3
guard, AND the `rem` exclusion structurally.

**CSS (2026-06-10):** uses Lexilla's `css` lexer
(`LexCSS.cxx`). The `L_CSS` row routes `.css` files to the
lexer with **five wordlist classes installed simultaneously** —
the broadest population in the framework so far (every other
wired row uses 0, 1, or 2 classes; ASP uses 3). Classes 0
(`CSS_PROPERTIES_CSS1`, 53 entries) + 2 (`CSS_PROPERTIES_CSS2`,
69 entries) + 3 (`CSS_PROPERTIES_CSS3`, 254 entries) +
(future) class 5 form a **five-arm IDENTIFIER cascade**
(`LexCSS.cxx:425-438`): CSS1 hit → `SCE_CSS_IDENTIFIER`,
CSS2 hit → `SCE_CSS_IDENTIFIER2`, CSS3 hit →
`SCE_CSS_IDENTIFIER3`, class 5 hit →
`SCE_CSS_EXTENDED_IDENTIFIER`, fallback →
`SCE_CSS_UNKNOWN_IDENTIFIER`. The "four-way" framing used
informally elsewhere in this note refers to the **four
populated arms** for v1 — class 5 is intentionally empty
(vendor-prefixed extensions, see next paragraph) so the
fifth arm is dead until a follow-up commit, but
`SCE_CSS_EXTENDED_IDENTIFIER` is still pre-themed
identically to the other three IDENTIFIER variants so a
future class-5 install picks up correct colouring with no
theme edit. Classes 1 (`CSS_PSEUDO_CLASSES`, 63 entries)
+ 4 (`CSS_PSEUDO_ELEMENTS`, 21 entries) drive
`SCE_CSS_PSEUDOCLASS` / `SCE_CSS_PSEUDOELEMENT` through
a separate cascade (`LexCSS.cxx:440-454`). Classes 5 / 6
/ 7 (vendor-prefixed extensions like `-webkit-*` /
`-moz-*`) intentionally left empty for v1 — cascade-miss to
`SCE_CSS_UNKNOWN_*` / `SCE_CSS_EXTENDED_*` is acceptable
until a follow-up commit lands browser-prefix wordlists.

Dedicated 20-mapping `CSS_STYLES` table — does NOT reuse
any other framework style table (CPP / HYPERTEXT /
MAKEFILE / PASCAL / BATCH / PROPS / SQL / VB — all 8
non-reuse assertions structurally pinned in the test). The
four-way IDENTIFIER cascade (`_IDENTIFIER` / `_IDENTIFIER2`
/ `_IDENTIFIER3` / `_EXTENDED_IDENTIFIER`) and the element
`SCE_CSS_TAG` (matching HTML's `SCE_H_TAG` precedent) ALL
map to Keyword bold blue so property colour stays uniform
regardless of which spec generation a property comes from —
distinct lexer-side indices exist for plugins, not for
human readers. CLASS / ID / PSEUDOCLASS / PSEUDOELEMENT /
ATTRIBUTE / VARIABLE / EXTENDED pseudo variants all → Keyword2
steel-blue (selector / variable accents). DIRECTIVE
(`@import` etc.) / GROUP_RULE (the four hard-coded `media`
/ `supports` / `document` / `-moz-document` per
`LexCSS.cxx:460-463`) / IMPORTANT (`!important`) → all
Preprocessor bold. COMMENT → Comment italic. OPERATOR /
String / Single+Double-string as expected. DEFAULT (0),
UNKNOWN_PSEUDOCLASS (4), UNKNOWN_IDENTIFIER (7), VALUE (8)
intentionally unmapped — fall through to STYLE_DEFAULT
(matches N++ light-theme convention; UNKNOWN_* are
wordlist-miss fallbacks not errors, VALUE is right-of-colon
literal text like `red` / `10px` / `auto` that N++ leaves
default-coloured).

**Critical: case-insensitive lexer.** `LexCSS.cxx:419` calls
`sc.GetCurrentLowered(s, ...)` on every candidate token
before any `WordList::InList` lookup. All five CSS
wordlists are stored all-lowercase. CSS source can use any
casing (`COLOR` / `Color` / `color` all match) — the
case-insensitive convention is honoured transparently;
uppercase wordlist entries would never match. The
`css_uses_lexcss_five_class_cascade_theme` test pins the
all-lowercase invariant on every wordlist structurally.

**Legitimate state-disambiguated cross-namespace overlaps.**
Unlike SQL / VB (strict no-overlap between class 0 / 1),
CSS has by-design cross-namespace duplicates that the lexer
state machine disambiguates: `left` and `right` appear in
both class 1 (paged-media pseudo-classes `:left` / `:right`
for print stylesheets) AND class 2 (positional properties
`left: 10px;`); `cue` appears in both class 2 (CSS2 aural
property `cue: ...`) AND class 4 (WebVTT pseudo-element
`::cue`). Lexilla disambiguates by lexer state — wordlist
queries fire only in the matching syntactic state. The test
pins these overlaps as REQUIRED invariants (not duplicates
to clean up) so a future "defensive" deduplication commit
can't silently break paged-media pseudos or `::cue`
styling.

**`opacity` MUST-FIX from adversarial verifiers.** Initial
synthesis omitted `opacity` (CSS Color Module Level 3,
2003) — both correctness and completeness verifiers
flagged it independently as the single highest-impact
omission. Added to class 3 before commit. Completeness
verifier also recommended modern v1 additions: `accent-color`
(form-control theming), `outline-offset` (CSS3 Basic UI),
`scrollbar-color` / `scrollbar-width` / `scrollbar-gutter`
(CSS Scrollbars Module), `content-visibility` (CSS
Containment Level 2), `font-display` (CSS Fonts L4
`@font-face` descriptor), `line-clamp` (formerly
`-webkit-line-clamp`, now standardised) — all added.
Structural pin in the test asserts `opacity` stays in
class 3 so a future cleanup doesn't drop it.

`css_uses_lexcss_five_class_cascade_theme` test pins the
20-mapping shape, five-class structure, canonical keyword
constant links, all-lowercase invariant on every wordlist,
strict no-overlap within the property-name cascade
(class 0 / 2 / 3), strict no-overlap within the pseudo
namespaces (class 1 / 4), the legitimate state-disambiguated
cross-namespace overlaps as REQUIRED invariants
(left/right in class 1 + class 2, cue in class 2 + class 4),
the four-way IDENTIFIER cascade uniform-bold theming, AND
the `opacity` structural pin.

**Perl (2026-06-10):** uses Lexilla's `perl` lexer
(`LexPerl.cxx`). The `L_PERL` row routes `.pl` / `.pm` /
`.cgi` files to the lexer with a single 259-entry
**mixed-case** wordlist installed at class 0 (the only
class `perlWordListDesc[]` declares). Most entries are
lowercase per standard Perl convention, but **13 entries
are stored UPPERCASE** because `LexPerl.cxx:96-104`
(`isPerlKeyword`) is byte-exact case-sensitive and Perl
source spells these uppercase by language requirement —
storing them lowercase would silently disable the
highlight. The uppercase subset: 7 phase-block special
subroutines (`BEGIN` / `END` / `INIT` / `CHECK` /
`UNITCHECK` / `AUTOLOAD` / `DESTROY`) + 6 `__TOKEN__`
markers (`__FILE__` / `__LINE__` / `__PACKAGE__` /
`__SUB__` / `__DATA__` / `__END__`). All 13 structurally
pinned in the dedicated Perl test.

**`__DATA__` / `__END__` uppercase entries are
load-bearing for `SCE_PL_DATASECTION` styling.**
`LexPerl.cxx:872-877` only recolours these markers (and
everything after them) to `SCE_PL_DATASECTION` from
inside the `SCE_PL_WORD` state, which is only entered
after a wordlist match. Without uppercase entries the
trailing data section never picks up de-emphasised paint.
Specifically pinned in the test as a load-bearing
invariant separate from the general uppercase pin.

Dedicated **38-mapping** `PERL_STYLES` table — the
**largest dedicated style table in the framework so far**
(prior max was CSS at 20). Does NOT reuse any other
framework style table (CPP / HYPERTEXT / MAKEFILE /
PASCAL / BATCH / PROPS / SQL / VB / CSS / RUST — all **10
non-reuse assertions** structurally pinned). Maps:
- COMMENT / POD / POD_VERB / DATASECTION → Comment italic
  (collapses line comments, POD prose, verbatim POD, and
  the trailing data section into the same "non-executable
  prose" archetype).
- NUMBER → Number, WORD → Keyword bold, STRING /
  CHARACTER → String, OPERATOR → Operator.
- **Sigil archetype** (4 styles): SCALAR / ARRAY / HASH /
  SYMBOLTABLE all → `Lifetime` — the "purple sigil-tagged
  identifier" reuse. Perl sigils (`$x` / `@x` / `%x` /
  `*x`) share the visual archetype with Rust lifetimes
  (`'a`): short identifier-decorator, distinct from both
  keywords and bare identifiers. Mapping all four
  uniformly keeps `$/@/%/*` variables visually consistent
  regardless of namespace.
- **Heredoc family** (4 styles): HERE_DELIM → Keyword2
  bold (the `<<EOF` opener is a structural marker);
  HERE_Q / HERE_QQ / HERE_QX → String (the body in
  single-quoted / interpolating / backtick variants).
- **q-family** (5 styles): STRING_Q / STRING_QQ /
  STRING_QX / STRING_QR / STRING_QW → String (all
  quote-operator bodies, regardless of interpolation
  semantics).
- REGEX / REGSUBST / BACKTICKS → String. **XLAT**
  (`tr/abc/xyz/` / `y/abc/xyz/`) → String.
- **`+37` INTERPOLATE_SHIFT band** (9 _VAR slots):
  STRING_VAR / REGEX_VAR / REGSUBST_VAR / BACKTICKS_VAR /
  HERE_QQ_VAR / HERE_QX_VAR / STRING_QQ_VAR /
  STRING_QX_VAR / STRING_QR_VAR all → `Lifetime`. These
  are interpolated `$var` / `@var` references INSIDE
  string / regex / heredoc / backticks bodies — the sigil
  archetype carries through so an interpolated variable
  reads the same purple as a top-level `$x` against the
  surrounding String body. `LexPerl.cxx:94` defines
  `INTERPOLATE_SHIFT = 37` (`SCE_PL_STRING_VAR -
  SCE_PL_STRING`); non-interpolating base states leave
  their `+37` slots unused (45-53, 56, 58-60, 63, 67 —
  note slot 44 is `SCE_PL_XLAT`, which IS used for `tr///`
  / `y///` transliteration bodies and is NOT part of the
  interpolation-shadow band).
- SUB_PROTOTYPE / FORMAT_IDENT → Keyword2 bold (the
  `(...)` sig in `sub NAME (...)` and the `NAME =` header
  in `format NAME = ...` are structural declarators);
  FORMAT → String (the picture-body template).

Intentionally unmapped — fall through to STYLE_DEFAULT:
DEFAULT (0), ERROR (1, pending `StyleSlot::Error`),
PUNCTUATION (8, "currently not used" per LexPerl source),
PREPROCESSOR (9, "preprocessor unused" — Perl has no real
preprocessor), IDENTIFIER (11, bare-identifier
fall-through matches `SCE_C_IDENTIFIER` /
`SCE_PAS_IDENTIFIER` precedent), VARIABLE_INDEXER (16,
"allocated but unused" per LexPerl source), LONGQUOTE
(19, "obsolete: replaced by qq/qx/qr/qw"). The
deferred-Error-slot migration list grows from 10 entries
(after CSS) to **11** with Perl's ERROR added.

**Adversarial-verifier MUST-FIX additions from
synthesis-round.** All three verifiers (correctness /
completeness / format) independently flagged the same
top issue: the synth claimed the wordlist contained the
phase-block + `__TOKEN__` family but the actual list
held none of them, and the proposed "lowercased into the
same wordlist" plan was self-defeating because LexPerl's
`WordList::InList` is byte-exact (no case folding). Fix:
13 UPPERCASE entries added before commit. Completeness
verifier additionally flagged the missing `ge`
string-comparison operator (lt / gt / le / ge / eq / ne
/ cmp was incomplete — `ge` was dropped); added. Format
verifier flagged three `StyleSlot::Default` references
in the synth's style_mapping_notes (the enum has no
Default variant — indices 0 / 1 / 11 should be unmapped
per framework convention); applied. Final wordlist: 245
+ 14 = 259 entries.

`perl_uses_lexperl_mixed_case_theme` test pins the
38-mapping shape, single-class structure, canonical
keyword constant link, the 13 UPPERCASE entries
structurally, the load-bearing `__DATA__` / `__END__`
markers specifically, the restored `ge` operator, the
four sigil-archetype Lifetime routings, every populated
_VAR Lifetime routing (9 entries), and the four bold
structural anchors (WORD / HERE_DELIM / SUB_PROTOTYPE /
FORMAT_IDENT).

**Python (2026-06-24):** uses Lexilla's `python` lexer
(`LexPython.cxx`). The `L_PYTHON` row routes `.py` files
with **two wordlist classes** installed (matches
`pythonWordListDesc[]`): `PYTHON_KEYWORDS` (37 entries,
class 0 = reserved + soft keywords) drives `SCE_P_WORD` →
Keyword bold, `PYTHON_KEYWORDS_2` (270 entries, class 1 =
highlighted identifiers) drives `SCE_P_WORD2` → Keyword2.

**Case-sensitive lexer.** `LexPython.cxx:671` calls
`keywords.InList(identifier)` with no case folding — zero
matches for `tolower` / `MakeLowerCase` /
`GetCurrentLowered` in the source. Wordlists store
source-canonical casing: `True` / `False` / `None`
capitalised (Python 3 reserved literals), lowercase rest
of class 0, CamelCase exception classes in class 1, dunder
underscores preserved on `__init__` / `__repr__` etc.

**`True` / `False` / `None` placement: class 0, NOT
class 1.** Python 3 makes these hard reserved words
(`True = 5` raises `SyntaxError`). Notepad++ historically
places them in class 1 (WORD2) for Python 2 backward
compatibility — Code++ deliberately diverges to honour
modern language semantics. Pinned in the test as a
structural invariant.

**`match` / `case` soft keywords (Python 3.10+).**
Installed in class 0. LexPython.cxx:258-289
(`IsMatchOrCaseIdentifier`) disambiguates non-pattern
context — `match = 1` and `obj.match()` correctly degrade
to `SCE_P_IDENTIFIER` while `match value:` / `case 1:` at
statement position fire `SCE_P_WORD`. The lexer does the
right thing; installing the soft keywords is correct and
safe.

**No cross-class duplicates.** Lexilla checks class 0
first (LexPython.cxx:671), so a duplicate would silently
demote class 1 entries — an invisible bug. Verified by
PowerShell HashSet intersection before commit (zero
overlap on 37 + 270 = 307 unique tokens) AND pinned
structurally by the Python theme test.

Dedicated **18-mapping** `PYTHON_STYLES` table — does NOT
reuse any other framework style table (CPP / HYPERTEXT /
MAKEFILE / PASCAL / BATCH / PROPS / SQL / VB / CSS / RUST
— all **10 non-reuse assertions** structurally pinned).
Maps:
- COMMENTLINE (`# ...`) / COMMENTBLOCK (`##` line prefix
  per LexPython.cxx:914) → Comment italic.
- NUMBER → Number, STRING / CHARACTER → String,
  OPERATOR → Operator.
- WORD (class 0 hit) → Keyword bold.
- WORD2 (class 1 hit) → Keyword2.
- TRIPLE (`'''...'''`) / TRIPLEDOUBLE (`"""..."""`) →
  String, not Comment. The lexer has no docstring-specific
  state; styling these as Comment would mis-colour
  multi-line SQL queries, `re.VERBOSE` regex patterns,
  embedded HTML / JSON test fixtures. Matches PyCharm / VS
  Code / Sublime / Notepad++ all painting triple-quoted
  literals as String.
- **F-string family** (4 styles): FSTRING (`f"..."`) /
  FCHARACTER (`f'...'`) / FTRIPLE (`f'''...'''`) /
  FTRIPLEDOUBLE (`f"""..."""`) all → String. The `{}`
  interpolation sub-lexer is internal to Lexilla.
  Activation is automatic — `stringsF = true` by default
  (LexPython.cxx:297), Code++ doesn't override.
- **Name-being-defined family**: CLASSNAME (post-`class `
  identifier) / DEFNAME (post-`def ` identifier) →
  Keyword2. The lexer's kwLast state machine
  (LexPython.cxx:673-676) auto-reclassifies the identifier
  after a `class` / `def` wordlist hit — no wordlist install
  needed for the names themselves. Code++ rides Keyword2's
  non-bold weight (N++ paints bold; palette discipline keeps
  the C++ `std::` / Rust `mod` / Python class-name accent
  visually consistent).
- DECORATOR (`@foo` at line start) → Preprocessor bold.
  Line-start gated by `IsFirstNonWhitespace`
  (LexPython.cxx:916) so mid-expression `@` matrix-mul
  (Python 3.5+) correctly degrades to `SCE_P_OPERATOR`.
- ATTRIBUTE (post-decorator attribute access) → Keyword2.
  **Pre-themed only**: `lexer.python.identifier.attributes`
  and `lexer.python.decorator.attributes` both default to 0
  (LexPython.cxx:305-306), Code++ never calls SetProperty.
  The state never fires under defaults; wired for
  forward-compat (same pattern as CSS EXTENDED_PSEUDOCLASS
  pre-theming).

Intentionally unmapped — fall through to STYLE_DEFAULT:
DEFAULT (0), IDENTIFIER (11, bare-identifier fall-through
matching `SCE_C_IDENTIFIER` / `SCE_PL_IDENTIFIER`
precedent), STRINGEOL (13, pending `StyleSlot::Error`).
The deferred-Error-slot migration list grows from 11
entries (after Perl ERROR) to **12** with Python's
STRINGEOL added.

`python_uses_lexpython_two_class_theme` test pins the
18-mapping shape, **10** non-reuse style-table assertions,
two-class structure, canonical keyword constant links,
strict no-overlap between classes, the `True` / `False` /
`None` class-0-only invariant (Python 3 reserved status),
the `self` / `cls` class-1-only invariant (conventional,
not reserved), the `match` / `case` class-0 invariant
(soft keywords, lexer disambiguates), the f-string family
→ String routing (4 indices), the name-being-defined
family → Keyword2 routing (4 indices including the
forward-compat ATTRIBUTE), the DECORATOR → Preprocessor
routing, the bold structural anchors (WORD + DECORATOR),
and both italic comment slots (COMMENTLINE + COMMENTBLOCK).

**Lua (2026-06-24):** uses Lexilla's `lua` lexer
(`LexLua.cxx`). The `L_LUA` row routes `.lua` files
(N++ `langs.model.xml` default-set parity — Lua row
ships `lua` only; `wlua` / `rockspec` / `nse` deferred
as plausible follow-on additions but NOT N++ defaults).

**8-class wordlist surface, 2 populated for m1.**
`luaWordListDesc[]` at LexLua.cxx:51-61 declares 8 slots:
class 0 "Keywords", class 1 "Basic functions", class 2
"String, (table) & math functions", class 3 "(coroutines),
I/O & system facilities", classes 4-7 "user1..user4".
Code++ m1 installs classes 0 + 1: `LUA_KEYWORDS` (22
entries, Lua 5.4 reserved words from the reference manual
§3.1) drives `SCE_LUA_WORD` → Keyword bold; `LUA_KEYWORDS_2`
(25 entries, basic library functions from `_G` per §6.1)
drives `SCE_LUA_WORD2` → Keyword2 steel-blue. Classes 2-7
are PRE-THEMED to Keyword2 in `LUA_STYLES` (all 7 secondary
slots route to the same palette colour) so a future
`LUA_KEYWORDS_3` (string + table + math member names) and
`LUA_KEYWORDS_4` (coroutine + io + os + debug member
names) need only a one-line `keywords:` array extension
in `LUA_THEME` to activate — same forward-compat pattern
as CSS EXTENDED_PSEUDOCLASS pre-theming and Python
ATTRIBUTE pre-theming. Classes 4-7 stay empty by design
(user customisation slots).

**Case-sensitive lexer.** LexLua.cxx:472, 479 calls
`keywords.InList(identifier)` with no case folding —
verified by inspection of `WordList::InList` at
`vendor/lexilla/lexlib/WordList.cxx:162-170, 202-204`
(byte-exact comparison with zero `tolower` /
`MakeLowerCase` / `CompareCaseInsensitive` anywhere on
the path). Identifier text captured raw via
`sc.GetCurrentString(s, Transform::none)` at
LexLua.cxx:391. Net result: `function` highlights as a
keyword, `Function` does not; `_G` and `_VERSION` match
the canonical Lua sentinel casing but `_g` does not.

**`goto` placement: class 0, load-bearing.** Two lexer
paths consult class 0 specifically for the keyword
`goto`. (1) The `goto target` label-from-goto-target path
at LexLua.cxx:382-396 tracks `idenStyle == SCE_LUA_WORD &&
ident == "goto"` to arm the next-identifier-is-LABEL
state — if `goto` is missing from class 0, the entire
construct silently never highlights `target_name` as
`SCE_LUA_LABEL`. (2) The `::label::` definition path at
LexLua.cxx:320-357 has a `!keywords.InList(s)` guard at
:335 rejecting `::reserved_word::` constructs as not-a-
label. Both paths require `goto` to live in class 0. The
class-0-only invariant for `goto` is structurally pinned
in the Lua theme test.

**`type` / `getmetatable` placement: class 1 ONLY.** Lua
has both `type(v)` (basic function) and `math.type` /
`io.type` (library member names). Class 1 owns the bare
`type`; a future `LUA_KEYWORDS_3` covering `math.*` /
`io.*` member names must NOT re-add it — Lexilla checks
class 0 first (LexLua.cxx:472, 479-480), then class 1,
then 2 in source order, and a cross-class duplicate
silently demotes the secondary entry. Same load-bearing
constraint for `getmetatable` (bare basic function vs.
`debug.getmetatable`).

**No cross-class duplicates.** Verified by HashSet
intersection before commit (zero overlap on 22 + 25 = 47
unique tokens) AND structurally pinned by the Lua theme
test.

Dedicated **18-mapping** `LUA_STYLES` table — does NOT
reuse any other framework style table (CPP / HYPERTEXT /
MAKEFILE / PASCAL / BATCH / PROPS / SQL / VB / CSS / RUST
— all **10 non-reuse assertions** structurally pinned).
Maps:
- COMMENT (`--[[ ]]` block) / COMMENTLINE (`-- ...` line,
  also catches the top-of-file shebang per
  LexLua.cxx:280) / COMMENTDOC (`---` LDoc-initiated,
  plus cross-line continuation tracked via the
  `lastLineDocComment` line-state flag at LexLua.cxx:534,
  542-544) all → Comment italic. **LDoc tag handling
  (`---@param`, `---@return`) is NOT a separate lexer
  state** — the entire run from `---` to EOL is one flat
  COMMENTDOC token, so a single Comment treatment covers
  all three slot variants.
- NUMBER → Number, OPERATOR → Operator. NUMBER
  recognises decimal, hex (`0x` / `0X` prefix), and
  hex-float (`p` / `P` binary exponent) per
  LexLua.cxx:240-242, 359-366.
- WORD (class 0 hit) → Keyword bold.
- WORD2 ... WORD8 (7 secondary classes) → Keyword2 — see
  forward-compat rationale above.
- STRING (`"..."`) / CHARACTER (`'...'`) / LITERALSTRING
  (`[[...]]` / `[=[...]=]` / up to 254 `=` chars per
  `LongDelimCheck` at LexLua.cxx:41-49, 525-532) all →
  String. Lua makes no semantic char/string split — both
  quote forms are functionally identical, differing only
  in which quote needs escaping.
- LABEL → Preprocessor. Structural anchor for `::name::`
  goto labels and `goto target` resolution targets per
  LexLua.cxx:320-396. Routing to Preprocessor matches
  Python's SCE_P_DECORATOR precedent (both are
  out-of-band annotation styles). **Bold-tagged**
  alongside WORD for visual weight matching the
  structural-anchor role.
- PREPROCESSOR → Preprocessor (NOT bold). LexLua.cxx:
  548-549 emits this ONLY for `$` at column 0 —
  obsolete since Lua 4.0 per the source comment. The
  `#!` shebang at file top is handled separately at
  LexLua.cxx:278-281 and types as COMMENTLINE, NOT
  PREPROCESSOR. Kept visually identifiable via the
  Preprocessor slot but excluded from the bold list —
  boldening dead syntax misleads.

Intentionally unmapped — fall through to STYLE_DEFAULT:
DEFAULT (0), IDENTIFIER (11, bare-identifier fall-through
matching `SCE_C_IDENTIFIER` / `SCE_P_IDENTIFIER` /
`SCE_PL_IDENTIFIER` precedent), STRINGEOL (12, pending
`StyleSlot::Error`). The deferred-Error-slot migration
list grows from 12 entries (after Python's STRINGEOL) to
**13** with Lua's STRINGEOL added.

`lua_uses_lexlua_eight_class_theme` test pins the
18-mapping shape, **10** non-reuse style-table assertions,
two-class structure (m1), canonical keyword constant
links, the no-class-2+-for-m1 structural guard, strict
no-overlap between classes, the seven reserved-word
class-0-only invariants (`function` / `local` / `then` /
`end` / `nil` / `true` / `false`), the load-bearing
`goto` class-0 invariant (SCE_LUA_LABEL emission
depends on it), the five basic-library class-1-only
invariants (`print` / `tostring` / `type` / `pairs` /
`getmetatable`), the comment-family → Comment routing
(3 indices), the string-family → String routing (3
indices including the long-bracket LITERALSTRING), the
SCE_LUA_WORD → Keyword routing, all seven secondary
WORD2..WORD8 → Keyword2 routings (forward-compat
pre-theming), the LABEL → Preprocessor routing
(out-of-band anchor), the PREPROCESSOR → Preprocessor
routing (legacy `$` directive), the bold structural
anchors (WORD + LABEL — note PREPROCESSOR deliberately
excluded), and all three italic comment slots
(COMMENT + COMMENTLINE + COMMENTDOC).

**TeX + LaTeX (2026-06-24):** paired wiring across two distinct
Lexilla lexers — `tex` (`LexTeX.cxx`, 6-state emission set) for
plain TeX (`L_TEX` row, extension `.tex`) and `latex`
(`LexLaTeX.cxx`, 13-state emission set) for LaTeX (`L_LATEX` row,
extension `.latex`). N++ ships both as separate language menu
entries; Code++ matches that. Neither language ships keyword
wordlists: `LATEX_THEME.keywords` is `&[]` because
`LexLaTeX.cxx:561` declares `emptyWordListDesc = {0}` and the
lexer never calls `keywords.InList`. `TEX_THEME.keywords` is
`&[]` by deliberate choice — `LexTeX.cxx:230-245` silently
downgrades unknown `\command` tokens to plain text when a
populated wordlist filters them, and the default `.tex` handler
in Code++ is `L_TEX` (not `L_LATEX`), so a user opening a `.tex`
file containing LaTeX content would see `\section` / `\textbf`
render as plain prose while only Knuth's `\def` / `\let`
highlighted — surprising visual feedback. Empty wordlist
short-circuits the filter and every `\command` paints uniformly
as `SCE_TEX_COMMAND` Keyword bold. Matches N++ default-set parity.

**Both lexers are case-sensitive.** LexTeX byte-exact-compares at
`:236`; LexLaTeX does byte-exact `strcmp` against lowercase
needles like `"\\begin"` / `"{verbatim}"` at `:158-193`. So
`\Begin{equation}` is not recognised as a tag and `\Section`
does not match LaTeX-the-language's section sectioning command.

**The load-bearing TeX-routing decision** is `SCE_TEX_DEFAULT`
→ Comment. Counter to the slot's name, `SCE_TEX_DEFAULT` is the
comment-body emission state per `LexTeX.cxx:248-254`: the
leading `%` is `SCE_TEX_SYMBOL`, then every subsequent char
until EOL paints `SCE_TEX_DEFAULT` while the `inComment` flag
is set. `SCE_TEX_TEXT` is the plain-prose fall-through (the
`StyleContext` initial state at `:202`) — left unmapped, it
renders as `STYLE_DEFAULT`. Both decisions structurally pinned
in the TeX theme test.

**LaTeX state doubling is theme-collapsed.** MATH (inline `$..$`
/ `\(..\)`) and MATH2 (display `$$..$$` / `\[..\]` / math
environments per `mathEnvs[]` at `LexLaTeX.cxx:116-129`) both
route to `StyleSlot::String` — math content is a literal region
semantically. COMMENT (`%`-to-EOL) and COMMENT2
(`\begin{comment}` / `\end{comment}` block) both route to
`StyleSlot::Comment`. TAG (`\begin{env}`) and TAG2
(`\end{env}`) both route to `StyleSlot::Keyword2` — environment
names are LaTeX-specific structural identifiers, matches the
`SCE_P_CLASSNAME` precedent. COMMAND (`\foo`) and SHORTCMD
(single-char `\\` / `\!` / `\,`) both route to `StyleSlot::Keyword`.
VERBATIM (`\verb<delim>` and `\begin{verbatim}` /
`\begin{lstlisting}`) routes to `StyleSlot::String` — verbatim
content is a literal region. SPECIAL (the eight escaped
characters `\#` / `\$` / `\%` / `\&` / `\_` / `\{` / `\}` /
`\<space>` per `latexIsSpecial`) routes to `StyleSlot::Operator`
— punctuation escapes. CMDOPT (the `[opt]` option block on
commands like `\section[short]{long}`) routes to
`StyleSlot::Keyword2` — structural identifier.

**Intentional omissions.** TeX: `SCE_TEX_TEXT` (plain-prose
fall-through). LaTeX: `SCE_L_DEFAULT` (plain-prose), `SCE_L_ERROR`
(deferred-Error-slot migration cluster). All three are explicitly
pinned as omissions in the theme tests.

**Italic + bold structural anchors.** TeX italic: `SCE_TEX_DEFAULT`
(comment body). TeX bold: `SCE_TEX_COMMAND`. LaTeX italic: both
comment families (`SCE_L_COMMENT` + `SCE_L_COMMENT2`; VERBATIM
stays roman because verbatim is typically monospace). LaTeX bold:
every command-shaped state (`SCE_L_COMMAND` + `SCE_L_SHORTCMD` +
`SCE_L_TAG` + `SCE_L_TAG2`) — same "keyword + structural anchor"
convention as Lua (`SCE_LUA_WORD` + `SCE_LUA_LABEL`).

**Excluded from `wired_languages_have_complete_themes`.** Both
languages have dedicated standalone tests instead. TeX has 5
style mappings — below the iteration's `>=8` floor (which
calibrates for richly-styled lexers like LexCPP and the
hypertext family; TeX's compact emission set is legitimate,
just smaller). LaTeX has 11 mappings (would pass the floor) but
empty `keywords` — violates the iteration's `!keywords.is_empty()`
guard, matches the `PROPS_THEME` precedent which is excluded for
the same reason. Each dedicated test pins the size, the **10**
non-reuse `assert_ne!` style-table assertions (CPP / MAKEFILE /
PASCAL / HYPERTEXT / BATCH / PROPS / SQL / VB / CSS / RUST), the
empty-keywords invariant, the style-slot routing for every
populated mapping, the explicit omissions for unmapped states,
and the italic + bold structural anchors. LaTeX additionally
pins the TeX cross-non-reuse (paired family but structurally
distinct themes — `latex.styles != tex.styles`).

**Extension parity.** `L_TEX` matches `.tex` only; `L_LATEX`
matches `.latex` only. Both deliberately conservative — N++
`langs.model.xml` defaults match the same. Plausible follow-on
additions tracked but not in this commit: `.sty` (LaTeX style
packages), `.cls` (LaTeX class files), `.ltx` / `.dtx` (LaTeX
variants), `.wtex` (Windows-Live-Writer TeX scratch files). NOT
adding `.bib` (BibTeX has its own grammar; lives in a future
`L_BIBTEX` row mapping to the separate LexBib lexer).

**Bash (2026-06-24):** uses Lexilla's `bash` lexer (`LexBash.cxx`)
— 14 emission states (0..=13) with a **single wordlist class**.
`bashWordListDesc[]` at `LexBash.cxx:205-208` declares exactly one
named slot (`"Keywords"`, `nullptr`-terminated); `LexerBash::
WordListSet` at `:558-572` only dispatches `case 0:` and no-ops
for any other class index — so reserved words and builtins
necessarily share class 0 (unlike Lua / Python / SQL which split
into 2+ classes). There is no `BASH_KEYWORDS_2`. Code++ populates
`BASH_KEYWORDS` with Bash builtins (`echo`, `printf`, `cd`,
`export`, `declare`, …) and reserved tokens NOT already handled
by the hard-wired `bashStruct` / `bashStruct_in` sets at
`LexBash.cxx:491-494` (`bashStruct = "if elif fi while until else
then do done esac eval"`, `bashStruct_in = "for case select"` —
matched independently of the user wordlist at `:706, :713`, so
duplicating them in `BASH_KEYWORDS` would be no-op spec noise).

**Case-sensitive byte-exact match.** `LexBash.cxx:727` calls
`keywords.InList(s)` against the raw `sc.GetCurrent(s, ...)`
buffer with no `MakeLowerCase` / `GetCurrentLowered` anywhere in
the lexer (verified by grep). Wordlist contents must be lowercase
to match Bash language semantics: `if`/`then`/`fi` are keywords,
`IF`/`Then`/`FI` fall through to `SCE_SH_IDENTIFIER`. Pinned
structurally in the theme test by the `!c.is_ascii_uppercase()`
guard on `BASH_KEYWORDS`.

**Key divergence from LexPerl: no `SCE_SH_HERE_QQ` / `SCE_SH_HERE_QX`.**
Where LexPerl splits heredoc bodies into `SCE_PL_HERE_Q` /
`SCE_PL_HERE_QQ` / `SCE_PL_HERE_QX` based on the delimiter's
quoting style, LexBash emits a single `SCE_SH_HERE_Q` (state 13)
for every body byte regardless of delimiter quoting. The
quoted-vs-unquoted distinction is tracked INTERNALLY via
`HereDocCls::Quoted` / `Escaped` flags at `LexBash.cxx:594-595`
and affects only nested-expansion suppression behaviour at
`:906-908` — the emitted style stays HERE_Q. The `Q` suffix is a
misnomer inherited from LexPerl's taxonomy. The scintilla-sys
banner explicitly forbids speculative declaration of
`SCE_SH_HERE_QQ` / `SCE_SH_HERE_QX` constants. Opening `<<EOF` /
`<<-EOF` delimiter line (and closing-delimiter line per `:896`)
gets `SCE_SH_HERE_DELIM` (state 12); here-string `<<<` is
consumed without a body state per `:828-830`.

**11-mapping style table.** DEFAULT (0), IDENTIFIER (8), and
ERROR (1) intentionally unmapped. DEFAULT + IDENTIFIER are the
universal-omission pattern (matches Perl / Python / Lua / Pascal
precedent — bare-default and post-keyword-miss render at
STYLE_DEFAULT). ERROR joins the **deferred-Error-slot migration
list** — the lexer emits it at `:792` for out-of-range base-N
digits, at `:862-864` for unterminated heredocs, and at `:792`
for malformed numerics. Synthesising an ad-hoc red here creates
palette drift; defer to the global Error-slot migration that
will sweep Perl ERROR + Lua / Python STRINGEOL + Bash ERROR +
the rest of the deferred cluster together.

**SCALAR + PARAM → Lifetime routing.** `SCE_SH_SCALAR` (9, bare
`$var` / `$1` / `$@`) and `SCE_SH_PARAM` (10, braced `${param}` /
`${param:-default}` parameter expansion) both route to
`StyleSlot::Lifetime` — direct precedent at `SCE_PL_SCALAR` /
`SCE_PL_ARRAY` / `SCE_PL_HASH` / `SCE_PL_SYMBOLTABLE → Lifetime`
in `PERL_STYLES`. The `Lifetime` slot is documented at the
`StyleSlot` enum (`crates/ui_win32/src/lib.rs:3074-3077`) as
reusable for "scoped binding" highlights; Bash's `$x` is the
canonical version of the sigil-tagged-variable archetype Perl
inherited from sh. Uniform routing across SCALAR + PARAM matches
Perl's uniform SCALAR/ARRAY/HASH/SYMBOLTABLE collapse.

**HERE_DELIM → Keyword2 + bold.** Structural anchor distinct
from the body — matches Perl `SCE_PL_HERE_DELIM → Keyword2 +
bold` precedent.

**Extension + filename expansion.** `L_BASH` claims `sh` /
`bash` (N++ default parity) plus `ksh` / `zsh` / `ash` / `dash`
(LexBash handles the POSIX shell + Bash extensions well enough
for these dialects' syntax highlighting — the divergences
tokenise gracefully). Filenames: `.bashrc` / `.bash_profile` /
`.bash_login` / `.bash_logout` / `.bash_aliases` / `.profile` /
`.zshrc` / `.zprofile` / `.zlogin` / `.zlogout` / `.zshenv` /
`.kshrc` plus `PKGBUILD` (Arch package script) and `configure`
(autoconf-generated). `.fish` deliberately omitted — Fish is not
POSIX-compatible, deserves its own `L_FISH` row if Lexilla ever
ships a fish lexer.

**Properties left at defaults.** `lexer.bash.styling.inside.*`
properties stay `false`, `lexer.bash.command.substitution` stays
`0` (default `Backtick`). Keeps emitted styles in the 0..=13
range and avoids the `commandSubstitutionFlag = 0x40` OR-shift
at `LexBash.cxx:92` that would produce styles in 64..=127. A
future property flip would require re-evaluating `BASH_STYLES`.

**Test included in `wired_languages_have_complete_themes`** —
11-mapping style table exceeds the 8-floor AND `BASH_KEYWORDS`
populates class 0, so both gates pass. Dedicated test
`bash_uses_lexbash_one_class_theme` additionally pins single-
class wordlist surface (no `BASH_KEYWORDS_2`), SCALAR/PARAM →
Lifetime routing, HERE_DELIM → Keyword2 routing, italic on
COMMENTLINE, bold on WORD + HERE_DELIM, no bold on SCALAR /
PARAM / BACKTICKS (sigil/expansion archetype carries weight via
colour slot — matches Perl SCALAR / ARRAY / HASH staying
non-bold), case-sensitive lowercase invariant, and 10 cross-
language non-reuse `assert_ne!` pins (CPP / Makefile / Pascal /
PHP / Batch / INI / SQL / VB / CSS / Perl). Test name
`_one_class_` is the first in the framework — distinguishes
single-populated-class from zero-class (TeX / LaTeX) and
two-class (Batch).

**NSIS (2026-06-24):** uses Lexilla's `nsis` lexer (`LexNsis.cxx`)
— 19 emission states (0..=18) with **four wordlist classes**.
`nsisWordLists[]` at `LexNsis.cxx:658-663` declares `"Functions"`
/ `"Variables"` / `"Lables"` [sic — upstream typo preserved] /
`"UserDefined"`, terminated by `nullptr`. Code++ populates classes
0 (`NSIS_FUNCTIONS` — ~200 entries covering NSIS instruction set
plus non-hard-wired `!`-directives plus plugin bare-name halves
like `nsExec` / `StrFunc`) and 1 (`NSIS_VARIABLES` — ~50 entries
covering predefined `$INSTDIR` / `$WINDIR` / shell-folder constants
plus the `$0..$9` / `$R0..$R9` numbered registers, each entry
prefixed with `$` per the lexer's identifier-buffer construction
at `LexNsis.cxx:252-265`). Classes 2 and 3 ship empty matching
N++'s `langs.model.xml` default — labels and user-defined macros
are user-extension surface, not host-shipped vocabulary.

**Hard-wired short-circuit cluster — 18 tokens bypass the
wordlist.** `classifyWordNsis` at `LexNsis.cxx:206-231`
short-circuits on `!macro` / `!macroend` / `!ifdef` / `!ifndef`
/ `!endif` / `!if` / `!else` / `!ifmacrodef` / `!ifmacrondef` /
`Section` / `SectionEnd` / `SubSection` / `SubSectionEnd` /
`SectionGroup` / `SectionGroupEnd` / `PageEx` / `PageExEnd` /
`Function` / `FunctionEnd` BEFORE consulting any user wordlist
— they route to dedicated `SCE_NSIS_MACRODEF` / `IFDEFINEDEF` /
`SECTIONDEF` / `SUBSECTIONDEF` / `SECTIONGROUP` / `PAGEEX` /
`FUNCTIONDEF` states instead of `SCE_NSIS_FUNCTION`. Pinned
structurally in the theme test by an `iter().any()` shadow guard
preventing accidental duplication; the test would catch a future
contributor copy-pasting `"section"` into `NSIS_FUNCTIONS` (it
would be unreachable spec noise — the lexer never consults the
wordlist for these tokens).

**Three-string-flavour collapse — STRINGDQ + STRINGLQ + STRINGRQ
all → String.** NSIS supports three independent quote characters:
`"..."` (DQ, state 2), `` `...` `` (LQ — "left quote", state 3),
`'...'` (RQ — "right quote", state 4). The three are tracked as
distinct lexer states (transitions at `LexNsis.cxx:322-326,
:327-334, :335-342` opening; `:388-407` closing) but collapse to
a single `StyleSlot::String` in `NSIS_STYLES` — uniform-archetype
matches the Lua `LITERALSTRING + CHARACTER + STRING` triple-
collapse precedent. String bodies support `$\` (dollar-backslash)
escape at `:385-386` so `$\"` does not close a DQ string, and a
trailing `\` at EOL at `:409-443` continues across lines.

**STRINGVAR + VARIABLE → Lifetime.** Sigil-tagged variable
archetype matching the Bash SCALAR / PARAM → Lifetime precedent.
`SCE_NSIS_STRINGVAR` (state 13) is emitted from inside an active
string body when the lexer detects `$var` / `${var}` / `$\esc` at
`:518, :527-530, :536` — same archetype as the bare top-level
variable, routes to the same slot for uniform visual handling.
The `${...}` brace-form at `:245-248` and the `nsis.uservars=1`
user-var fallback at `:252-266` (when the property is set) both
route to bare `SCE_NSIS_VARIABLE` outside strings.

**LABEL → Preprocessor.** Matches `SCE_LUA_LABEL` precedent. NSIS
labels are jump targets (`goto label_name`) inside Sections /
Functions; the `Preprocessor` slot's distinct colour carries the
"structural anchor, not content" cue without the visual noise that
bolding would create inside long Section bodies. The lexer does
NOT auto-detect "identifier followed by `:`" — user enumeration in
class 2 is required for labels to highlight (N++ ships class 2
empty, so Code++ matches; users opt in by editing config).

**USERDEFINED → Keyword2.** Matches `SCE_LUA_WORD2` precedent for
"secondary library / user customisation". User-defined names from
`!define foo bar` / `!macro mymacro` get this slot when the user
adds them to class 3 — N++ ships class 3 empty, Code++ matches.

**18-mapping style table.** DEFAULT (0) intentionally unmapped
(universal background-fall-through pattern). NO `SCE_NSIS_ERROR`
state exists in the lexer — `LexNsis.cxx` has no recovery /
malformed-token branch; the lexer simply walks back to DEFAULT on
any unmatched character. Contrast with `SCE_SH_ERROR` / `SCE_PL_ERROR`
which join the deferred-Error-slot migration cluster — NSIS doesn't
need a deferred-Error entry.

**Italic on both comment families.** `SCE_NSIS_COMMENT` (state 1,
`;` and `#` line comments per `:316`) AND `SCE_NSIS_COMMENTBOX`
(state 18, `/* ... */` block comments per `:357-361, :490-495`).
Matches the Lua COMMENT + COMMENTLINE + COMMENTDOC triple-italic
precedent (every comment-class state gets italic). Pinned to
exactly 2 entries by the theme test's `nsis.italic.len() == 2`
assertion — NSIS has no doc-comment third family to add.

**Bold on the structural + preprocessor cluster.** Eight states
bolded: FUNCTION + FUNCTIONDEF + SECTIONDEF + SUBSECTIONDEF +
SECTIONGROUP + PAGEEX get keyword-bold matching N++ default
scheme; IFDEFINEDEF + MACRODEF mirror the `SCE_C_PREPROCESSOR`
bold precedent for `#ifdef` / `#define`. LABEL deliberately NOT
bolded (Preprocessor colour carries the cue; matches
`SCE_LUA_PREPROCESSOR` staying non-bold); VARIABLE / STRINGVAR /
USERDEFINED deliberately NOT bolded (Lifetime / Keyword2 colours
carry the cue; matches Bash SCALAR / PARAM precedent).

**Canonical mixed-case wordlists, lexer-default property posture.**
LexNsis exposes two case + scope-modifying properties at `:178,
:184` whose lexer defaults are both `0`:

* `nsis.ignorecase` — when `1`, lowercases the buffered token
  before `InList` at `:198-202` AND routes all hard-wired matches
  through `NsisCmp` (`CompareCaseInsensitive`). When `0` (the
  default), `InList` runs byte-exact against the source spelling
  and the hard-wired branches `strcmp` directly against
  `Section` / `Function` / `!macro` / etc.
* `nsis.uservars` — when `1`, any `$`-prefixed token of valid
  `isNsisChar` characters classifies as `SCE_NSIS_VARIABLE`
  even outside the class-1 wordlist (`:252-266`). When `0`
  (the default), user-declared variables (`Var MyVar` →
  `$MyVar`) lex as `SCE_NSIS_DEFAULT` — only the predefined
  names in `NSIS_VARIABLES` highlight.

Code++ stores `NSIS_FUNCTIONS` and `NSIS_VARIABLES` in their
**canonical mixed-case** form per the NSIS Users Manual
(`MessageBox` / `SetOutPath` / `WriteRegStr` / `$INSTDIR` /
`$WINDIR` / `$R0..$R9` / etc.) — matching the source spelling an
NSIS author writes and matching the hard-wired-branch comparison
strings at `:206-231`. With both properties at their lexer default
(Code++ does not install them — `LangTheme` has no `properties`
slot today), the canonical-mixed-case wordlists match real NSIS
source byte-for-byte: `MessageBox` in the source hits the
mixed-case `MessageBox` in `NSIS_FUNCTIONS`, the hard-wired
`Section` matches the `:221` branch directly, and `$INSTDIR`
hits `$INSTDIR` in `NSIS_VARIABLES`. The remaining gap is
`nsis.uservars=0` — user-declared variables don't highlight.
Tracked as a follow-up that adds the `properties: &[(&str, &str)]`
slot to `LangTheme` and threads `SCI_SETPROPERTY` through
`apply_lang` — the same plumbing unlocks CSS's
`lexer.css.scss.language` and Python's
`lexer.python.identifier.attributes` forward-compat hooks already
referenced in their scintilla-sys banners, best landed as one
generalising commit rather than NSIS-specific. Once the slot
lands, only `nsis.uservars=1` needs installing — `nsis.ignorecase`
can stay at the lexer default because canonical mixed-case keeps
working under both property values. The 80% ✅ gate in this
matrix counts NSIS as ✅ because the wordlist + style + italic +
bold wiring is complete.

**Test included in `wired_languages_have_complete_themes`** —
18-mapping style table exceeds the 8-floor AND `NSIS_FUNCTIONS`
populates class 0, so both gates pass. Dedicated test
`nsis_uses_lexnsis_four_class_theme` additionally pins:
two-class wordlist install (classes 2 + 3 NOT installed —
structural guard matches N++ default-empty); the SEVEN dedicated
structural-state routings that the host MUST theme explicitly
(SECTIONDEF / SUBSECTIONDEF / SECTIONGROUP / PAGEEX / FUNCTIONDEF
→ Keyword, IFDEFINEDEF / MACRODEF → Preprocessor); the
three-string-flavour collapse to String; SCALAR-archetype
VARIABLE / STRINGVAR → Lifetime; LABEL → Preprocessor;
USERDEFINED → Keyword2; italic on both comment families; bold
on the structural + preprocessor cluster with non-bold pins for
LABEL / VARIABLE / STRINGVAR / USERDEFINED; canonical-
mixed-case anchor pins for both `NSIS_FUNCTIONS` (`MessageBox`,
`SetOutPath`, `WriteRegStr`, `CreateDirectory`) and
`NSIS_VARIABLES` (`$INSTDIR`, `$WINDIR`, `$PROGRAMFILES`, `$R0`)
matching the lexer-default `nsis.ignorecase=0` byte-exact
contract; `$`-sigil-prefix invariant for class 1 (the lexer
constructs the token buffer including the leading `$` at
`:252-265`); hard-wired-shadow guard preventing accidental
duplication of the 18 hard-wired tokens (`Section`,
`SectionEnd`, `Function`, …) into `NSIS_FUNCTIONS`; and 10
cross-language non-reuse `assert_ne!` pins. Test name
`_four_class_` is the first in the framework — distinguishes
the four-slot LexNsis surface from `_one_class_` (Bash),
`_two_class_` (Batch), and `_zero_class_` (TeX / LaTeX).

**TCL (2026-06-24):** uses Lexilla's `tcl` lexer (`LexTCL.cxx`) —
22 emission states (0..=21) with **nine wordlist classes**, the
richest wordlist surface in the framework. `tclWordListDesc[]` at
`LexTCL.cxx:361-372` declares `"TCL Keywords"` / `"TK Keywords"`
/ `"iTCL Keywords"` / `"tkCommands"` / `"expand"` / `"user1"` /
`"user2"` / `"user3"` / `"user4"`, terminated by `0`. Code++
populates classes 0-3 — `TCL_KEYWORDS` (class 0, ~100 entries
covering TCL 8.6 / 9.0 built-in commands per the Tcl Reference
Manual: variable / scope / namespace, control flow, procedure,
string / regex / format, I/O / channel, file system, process /
system, math / binary, dictionary), `TCL_TK_KEYWORDS` (class 1,
~50 entries covering Tk widget-creation commands and `tk_*`
dialog / utility commands per the Tk Reference Manual),
`TCL_ITCL_KEYWORDS` (class 2, ~20 entries covering `[incr Tcl]`
and TclOO class-body keywords and entry-point commands —
`oo::class` / `oo::define` / `oo::object`, `self` / `next` /
`my`, `method` / `superclass` / `mixin`), and `TCL_TK_COMMANDS`
(class 3, ~20 entries covering Tk geometry / event / introspection
commands per the Tk Reference Manual — `pack` / `grid` / `bind` /
`winfo` / `wm` / `focus` / `grab` / `event` / `font`). Classes
4-8 ship empty matching N++'s `langs.model.xml` default — class 4
(`expand`) is the special brace-context-only class for TCL `{*}`
expansion sentinels, and classes 5-8 (`user1..user4`) are
user-customisation slots.

**Case-sensitive byte-exact match.** No `MakeLowerCase` /
`tolower` / `GetCurrentLowered` / `CompareCaseInsensitive`
anywhere on the wordlist-match path — `keywords.InList(s)` at
`LexTCL.cxx:160-179` runs byte-exact against the source token
captured raw via `sc.GetCurrent(w, sizeof(w))` at `:152`. TCL
the language is case-sensitive at the interpreter level (`set`
and `SET` are distinct commands), so the lexer's byte-exact
posture matches TCL semantics. All four populated wordlists
store source-canonical lowercase spellings per the Tcl/Tk
Reference Manuals. Same byte-exact contract as `LUA_KEYWORDS`
/ `PERL_KEYWORDS`. The theme test pins canonical-lowercase
anchors structurally (`puts`, `set`, `if`, `proc`, `foreach`,
`while`, `expr` for class 0; `button`, `label`, `entry`,
`frame`, `text`, `canvas` for class 1; `class`, `method`,
`constructor`, `destructor` for class 2; `pack`, `grid`,
`place`, `bind`, `winfo`, `wm` for class 3) so a future
"let's uppercase the list" regression trips CI.

**Asymmetric class precedence — classes 5-8 override classes
0-3.** The lexer's match chain at `LexTCL.cxx:160-180` runs
classes 0-4 in a first-match-wins `if / else if` chain, then
runs classes 5-8 UNCONDITIONALLY after. A token duplicated
between class 0 and class 5 hits class 0 first BUT class 5
then overrides — the final classification is class 5. Code++
ships classes 5-8 empty so the override doesn't fire, but the
test's `HashSet` no-overlap guard structurally pins against
ANY duplication across the four populated wordlists to
future-proof the contract: any contributor adding `puts` to
both class 0 and class 2 trips CI immediately.

**Namespace-stripped match.** `IsAWordChar` at
`LexTCL.cxx:32-35` accepts `:` (the namespace separator), so
a fully-qualified `namespace::cmd` traverses as a single
identifier token through the wordlist. The lexer additionally
strips leading `:` from the candidate at `:156-157`, so `::set`
in source matches the bare `set` wordlist entry. To highlight
namespaced commands like `oo::class` requires the full
`namespace::cmd` form in the wordlist — which is exactly how
`TCL_ITCL_KEYWORDS` ships the TclOO entry points.

**Rich 20-mapping style table.** 22 emission states minus
DEFAULT (0) and IDENTIFIER (7), both following the universal-
omission pattern (background-text and bare-identifier render
at `STYLE_DEFAULT`). Routing summary: four-state comment cluster
(`COMMENT` + `COMMENTLINE` + `COMMENT_BOX` + `BLOCK_COMMENT`)
→ Comment uniform-collapse; two-state string-family
(`IN_QUOTE` + `WORD_IN_QUOTE` — the latter being LexTCL's
single mid-string keyword-hit slot per `:158-167` regardless
of which class matched) → String uniform-collapse;
`SUBSTITUTION` + `SUB_BRACE` → Lifetime (sigil-tagged variable
archetype — Bash SCALAR / PARAM precedent); `MODIFIER`
(`-flag` command-option) → Keyword2 (steel blue);
`EXPAND` (brace-context `{keyword}` class) → Keyword bold;
`WORD` (primary built-in commands) → Keyword bold;
`WORD2..WORD8` → Keyword2 (six secondary classes — Tk widgets,
iTcl / TclOO, tkCommands, and four user-customisation slots
pre-themed for forward-compat). No `SCE_TCL_ERROR` exists in
the lexer (no recovery / malformed-token branch), so no
deferred-Error-slot entry is needed (contrast with `SCE_SH_ERROR`
/ `SCE_LUA_STRINGEOL` joining the deferred cluster).

**Italic on all four comment families.** Richest comment surface
in the framework — `SCE_TCL_COMMENT` (command-position `#`),
`SCE_TCL_COMMENTLINE` (elsewhere `#`), `SCE_TCL_COMMENT_BOX`
(`#-` / `##` line-leading boxed continuation with cross-line
state at `:105, :220, :226, :286`), `SCE_TCL_BLOCK_COMMENT`
(`#~` at line-start at `:284`). All four italic, all four →
Comment slot. Test pins `tcl.italic.len() == 4` structurally.

**Bold on WORD + EXPAND only.** Two states bolded —
`SCE_TCL_WORD` (primary built-in command class, matches
SCE_SH_WORD / SCE_NSIS_FUNCTION precedent) and `SCE_TCL_EXPAND`
(brace-context expansion keyword class, structurally a TCL
keyword under the `{*}` mechanism). `WORD2..WORD8` deliberately
NOT bolded (their distinct steel-blue Keyword2 colour already
carries the cue; bolding the Tk + iTCL + tkCommands + user1..4
bands alongside core TCL keywords would create excessive visual
weight in Tk-heavy GUI scripts — same restraint as Perl's
SCE_PL_HASH / SCE_PL_ARRAY staying non-bold). `MODIFIER` /
`SUBSTITUTION` / `SUB_BRACE` deliberately NOT bolded — flags
appear densely (`string match -nocase -- $foo`), and the
Lifetime colour on sigil-tagged variables already carries the
cue (matches Bash SCALAR / PARAM staying non-bold).

**Property posture — lexer defaults.** LexTCL exposes two
runtime properties at `:51-52` via the legacy `GetPropertyInt`
API: `fold.comment` (default 0, off) and `fold.compact`
(default 1, on). Both affect folding only — neither changes
token emission. Code++ runs both at the lexer default (same
posture as NSIS — `LangTheme` has no `properties` slot today),
which is the correct shape for token-rendering parity. The
deferred properties-slot follow-up referenced in the NSIS row
generalises across TCL too, but folding behaviour is not the
gating concern (no token-emission impact). Tracked here for
the future folding-host wiring commit.

**Test included in `wired_languages_have_complete_themes`** —
20-mapping style table exceeds the 8-floor AND four wordlist
classes are installed, so both gates pass. Dedicated test
`tcl_uses_lextcl_nine_class_theme` additionally pins:
four-class wordlist install (classes 4-8 NOT installed —
structural guard matches N++ default-empty); the `HashSet`
no-overlap invariant across the four populated wordlists (the
override semantics of classes 5-8 make any duplicate
unreachable AND a future class-5 override would silently
demote a class-0 hit — the guard prevents both); the 20
populated style-routing pins; DEFAULT + IDENTIFIER
explicit-omission pins; italic on the four-comment-family
cluster with `len() == 4` pin; bold on WORD + EXPAND with
`len() == 2` pin and explicit non-bold guards for WORD2..WORD8
+ SUBSTITUTION + SUB_BRACE + MODIFIER; canonical-lowercase
anchor pins for all four wordlists matching the lexer's
byte-exact case-sensitive contract; structural no-overlap
pins on canonical anchors (no Tk widget-creation commands in
TCL_KEYWORDS; no Tk-management commands in TCL_TK_KEYWORDS; no
core TCL commands in TCL_ITCL_KEYWORDS); and 10 cross-language
non-reuse `assert_ne!` pins. Test name `_nine_class_` is the
first in the framework — distinguishes the nine-slot LexTCL
surface from `_one_class_` (Bash), `_two_class_` (Batch),
`_four_class_` (NSIS), and `_zero_class_` (TeX / LaTeX).

**Makefile (2026-05-14):** uses Lexilla's `makefile` lexer
(`LexMake.cxx`) — a small line-oriented lexer with a compact
5-style table and a single keyword class. `MAKEFILE_KEYWORDS`
(17 entries, all-lowercase) covers GNU Make directives recognised
as the first word on a line: conditional (`ifdef` / `ifndef` /
`ifeq` / `ifneq` / `else` / `endif`), define / undefine
(`define` / `endef` / `undefine`), include (`include` /
`sinclude` — `-include` excluded since the lexer rejects the
leading hyphen), visibility (`override` / `export` / `unexport`
/ `private`), path + dynamic-extension (`vpath` / `load`).
NMAKE `!`-prefixed directives, built-in functions (`call` /
`eval` / `foreach` / `shell` / etc.), automatic variables
(`$@` / `$<` / etc.), and special targets (`.PHONY` / etc.)
deliberately excluded — none drive wordlist lookups.

`MAKEFILE_STYLES` is the framework's first **non-shared,
compact** style table — does NOT reuse `CPP_STYLES` or
`HYPERTEXT_STYLES`. Five emission mappings: `COMMENT` →
Comment, `PREPROCESSOR` → Preprocessor (directives + NMAKE
`!`-prefixed lines), `IDENTIFIER` → Keyword2 (`$(VAR)`
references), `OPERATOR` → Operator, `TARGET` → Keyword (build
target names — bold blue, like function declarations in code
lexers). `DEFAULT` (0) falls through to STYLE_DEFAULT; `IDEOL`
(9, unclosed variable-reference error) unmapped pending future
`StyleSlot::Error`. Authored by a 7-agent workflow; all three
verifiers APPROVE. Not added to
`wired_languages_have_complete_themes` (its >= 8 style floor
fits LexCPP / hypertext families, not LexMake's compact
table); dedicated `makefile_uses_lexmake_compact_theme` test
pins the canonical wiring instead.

**XML (2026-05-14):** uses Lexilla's `xml` lexer (`lmXML` — same
factory family as `hypertext`, constructed with `isXml=true`).
Shares the same `HYPERTEXT_STYLES` / `HYPERTEXT_ITALIC` /
`HYPERTEXT_BOLD` tables as PHP / HTML. **Class 0 is empty by
design** — XML has no canonical element vocabulary, every
document defines its own via DTD or schema. Adding speculative
HTML tag entries would mis-colour user-defined elements as
known tags. Matches what Notepad++ / Visual Studio / IntelliJ /
VS Code all ship for XML. **Class 5** (`XML_KEYWORDS`, 20
entries, all-UPPERCASE) is the SGML / DTD vocabulary that
appears inside `<!DOCTYPE [ ... ]>` blocks: markup-declaration
keywords (`DOCTYPE` / `ELEMENT` / `ATTLIST` / `ENTITY` /
`NOTATION`), content-model + attribute-type keywords (`EMPTY` /
`ANY` / `CDATA` / `ID` / `IDREF` / `IDREFS` / `NMTOKEN` /
`NMTOKENS` / `ENTITIES` / `NUTOKEN`), external identifier +
conditional section keywords (`PUBLIC` / `SYSTEM` / `NDATA` /
`INCLUDE` / `IGNORE`). Hash-prefixed forms (`#PCDATA` /
`#REQUIRED` / `#IMPLIED` / `#FIXED`) deliberately excluded —
the lexer styles them via `SCE_H_SGML_SPECIAL`.

**`HYPERTEXT_STYLES` extended with the SGML range** in the same
commit: 8 new `SCE_H_SGML_*` mappings cover the DTD-block
sub-language (COMMAND → Keyword, 1ST_PARAM → Keyword2,
DOUBLESTRING / SIMPLESTRING → String, SPECIAL / ENTITY →
Preprocessor, COMMENT / 1ST_PARAM_COMMENT → Comment). DEFAULT
(21), ERROR (26), and BLOCK_DEFAULT (31) intentionally
unmapped — matches the existing `SCE_H_DEFAULT` /
`SCE_HPHP_DEFAULT` omission pattern (fall through to
STYLE_DEFAULT) plus pending future `StyleSlot::Error`. The
extension benefits HTML too — every `<!DOCTYPE html>` line at
the top of HTML files now gets DTD-keyword styling. Authored
by a 7-agent research-and-adversarial-verify workflow; all
three verifiers APPROVE (correctness with one info-level
warn about `NUTOKEN` being SGML-only rather than XML 1.0, kept
for Notepad++ baseline parity).

**HTML (2026-05-14):** rides the same hypertext lexer and the same
shared `HYPERTEXT_STYLES` / `HYPERTEXT_ITALIC` / `HYPERTEXT_BOLD`
tables already wired during the PHP commit. Single class 0 install
of the canonical `HTML_KEYWORDS` list — same shared wordlist PHP
uses for the HTML wrapper around its `<?php ?>` blocks. The list
was expanded as part of this commit from ~115 to 140 entries:
adds the full deprecated-but-still-supported HTML4 / Netscape-era
tag set (`acronym` / `applet` / `basefont` / `big` / `blink` /
`center` / `dir` / `font` / `frame` / `frameset` / `isindex` /
`keygen` / `listing` / `marquee` / `menuitem` / `nobr` / `noembed`
/ `noframes` / `param` / `plaintext` / `rb` / `rtc` / `spacer` /
`strike` / `tt` / `xmp`) plus `math` (MathML root, sibling of
`svg` as a foreign-content entry point). The expansion benefits
PHP files containing legacy HTML too. HTML attribute names
deliberately excluded — `SCE_H_ATTRIBUTE` and
`SCE_H_ATTRIBUTEUNKNOWN` both map to `StyleSlot::Keyword2` today,
so adding ~330 attribute identifiers would have no visible effect.
Embedded `<script>` JavaScript and `<style>` CSS deferred until
`L_JAVASCRIPT` / `L_CSS` rows are wired (same scope discipline as
PHP's `SCE_HJ_*` / `SCE_HB_*` deferral). Authored by a 7-agent
research-and-adversarial-verify workflow; all three verifiers
APPROVE with no blockers or warnings.

**Resource file / `.rc` (2026-05-14):** Win32 resource scripts —
declarative syntax for dialogs / menus / string tables / version
info / icons / etc. Uses the same `LexCPP` lexer and the same
shared `CPP_STYLES` / `CPP_ITALIC` / `CPP_BOLD` table as the rest
of the family, but is the **first single-class** LexCPP-family
theme: RC has no primitive-type vocabulary worth splitting, so
class 1 is intentionally unset. `RC_KEYWORDS` (class 0, 84
entries, all-UPPERCASE per RC convention) covers eight categories:
resource-type declarators (`DIALOG` / `DIALOGEX` / `MENU` /
`MENUEX` / `STRINGTABLE` / `VERSIONINFO` / `TOOLBAR` /
`DESIGNINFO` / etc.), block delimiters (`BEGIN` / `END`), 19
dialog control statements (`DEFPUSHBUTTON` / `LTEXT` /
`EDITTEXT` / etc.), dialog/resource attributes (`CAPTION` /
`STYLE` / `LANGUAGE` / etc.), menu words (`MENUITEM` / `POPUP`
/ `CHECKED` / `GRAYED` / etc.), accelerator flags (`VIRTKEY` /
`ASCII` / `ALT` / etc.), VERSIONINFO sub-statements
(`FILEVERSION` / `PRODUCTVERSION` / etc.), and legacy memory
attributes (`DISCARDABLE` / `MOVEABLE` / etc.). Library
constants from `windows.h` (`WS_*` / `DS_*` / `IDOK` / etc.)
deliberately omitted — they're identifiers, not RC keywords.
Authored by a 7-agent research-and-adversarial-verify workflow;
the correctness verifier flagged `USER` and `DLGINIT` (dropped
— not real source-level keywords), and the completeness
verifier added `DESIGNINFO` / `TOOLBAR` / `BUTTON`.

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
| ASP | 16 | `hypertext` | ✅ | ✅ | ✅ |
| Assembly | 32 | `asm` | ✅ | ✅ | ✅ |
| AutoIt | 40 | `au3` | ⚫ | ⚫ | 🟡 |
| AviSynth | 66 | `avs` | ⚫ | ⚫ | 🟡 |
| BaanC | 60 | `baan` | ⚫ | ⚫ | 🟡 |
| Batch | 12 | `batch` | ✅ | ✅ | ✅ |
| Blitzbasic | 67 | `blitzbasic` | ⚫ | ⚫ | 🟡 |
| C | 2 | `cpp` | ✅ | ✅ | ✅ |
| C# | 4 | `cpp` | ✅ | ✅ | ✅ |
| C++ | 3 | `cpp` | ✅ | ✅ | ✅ |
| Caml | 41 | `caml` | ⚫ | ⚫ | 🟡 |
| CMake | 48 | `cmake` | ⚫ | ⚫ | 🟡 |
| COBOL | 50 | `COBOL` | ⚫ | ⚫ | 🟡 |
| CoffeeScript | 56 | `coffeescript` | ⚫ | ⚫ | 🟡 |
| CSound | 70 | `csound` | ⚫ | ⚫ | 🟡 |
| CSS | 20 | `css` | ✅ | ✅ | ✅ |
| D | 52 | `d` | ⚫ | ⚫ | 🟡 |
| Diff | 33 | `diff` | ✅ | ✅ | ✅ |
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
| HTML | 8 | `hypertext` | ✅ | ✅ | ✅ |
| INI file | 13 | `props` | — | ✅ | ✅ |
| Inno Setup | 46 | `inno` | ⚫ | ⚫ | 🟡 |
| Intel HEX | 62 | `ihex` | ⚫ | ⚫ | 🟡 |
| Java | 6 | `cpp` | ✅ | ✅ | ✅ |
| Javascript | 58 | `cpp` | ⚫ | ⚫ | 🟡 |
| JSON | 57 | `json` | ⚫ | ⚫ | 🟡 |
| JSON5 | 94 | `json` | ⚫ | ⚫ | 🟡 |
| JSP | 55 | `hypertext` | ⚫ | ⚫ | 🟡 |
| KIXtart | 39 | `kix` | ⚫ | ⚫ | 🟡 |
| LaTeX | 74 | `latex` | ✅ | ✅ | ✅ |
| Lisp | 30 | `lisp` | ✅ | ✅ | ✅ |
| Lua | 23 | `lua` | ✅ | ✅ | ✅ |
| Makefile | 10 | `makefile` | ✅ | ✅ | ✅ |
| Matlab | 44 | `matlab` | ⚫ | ⚫ | 🟡 |
| MMIXAL | 75 | `mmixal` | ⚫ | ⚫ | 🟡 |
| Nim | 76 | `nim` | ⚫ | ⚫ | 🟡 |
| Nncrontab | 77 | `nncrontab` | ⚫ | ⚫ | 🟡 |
| NSIS | 28 | `nsis` | ✅ | ✅ | ✅ |
| Objective-C | 5 | `cpp` | ✅ | ✅ | ✅ |
| OScript | 78 | `oscript` | ⚫ | ⚫ | 🟡 |
| Pascal | 11 | `pascal` | ✅ | ✅ | ✅ |
| Perl | 21 | `perl` | ✅ | ✅ | ✅ |
| PHP | 1 | `hypertext` | ✅ | ✅ | ✅ |
| PostScript | 35 | `ps` | ✅ | ✅ | ✅ |
| PowerShell | 53 | `powershell` | ⚫ | ⚫ | 🟡 |
| Properties | 34 | `props` | — | ✅ | ✅ |
| Purebasic | 68 | `purebasic` | ⚫ | ⚫ | 🟡 |
| Python | 22 | `python` | ✅ | ✅ | ✅ |
| R | 54 | `r` | ⚫ | ⚫ | 🟡 |
| Raku | 89 | `raku` | ⚫ | ⚫ | 🟡 |
| REBOL | 79 | `rebol` | ⚫ | ⚫ | 🟡 |
| Registry | 80 | `registry` | ⚫ | ⚫ | 🟡 |
| Resource file | 7 | `cpp` | ✅ | ✅ | ✅ |
| Ruby | 36 | `ruby` | ⚫ | ⚫ | 🟡 |
| Rust | 81 | `rust` | ✅ | ✅ | ✅ |
| S-Record | 61 | `srec` | ⚫ | ⚫ | 🟡 |
| SAS | 91 | `sas` | ⚫ | ⚫ | 🟡 |
| Scheme | 31 | `lisp` | ✅ | ✅ | ✅ |
| Shell | 26 | `bash` | ✅ | ✅ | ✅ |
| Smalltalk | 37 | `smalltalk` | ⚫ | ⚫ | 🟡 |
| Spice | 82 | `spice` | ⚫ | ⚫ | 🟡 |
| SQL | 17 | `sql` | ✅ | ✅ | ✅ |
| Swift | 64 | `cpp` | ⚫ | ⚫ | 🟡 |
| TCL | 29 | `tcl` | ✅ | ✅ | ✅ |
| Tektronix extended HEX | 63 | `tehex` | ⚫ | ⚫ | 🟡 |
| TeX | 24 | `tex` | ✅ | ✅ | ✅ |
| TOML | 90 | `toml` | ⚫ | ⚫ | 🟡 |
| txt2tags | 83 | `txt2tags` | ⚫ | ⚫ | 🟡 |
| TypeScript | 85 | `cpp` | ⚫ | ⚫ | 🟡 |
| Verilog | 43 | `verilog` | ⚫ | ⚫ | 🟡 |
| VHDL | 38 | `vhdl` | ⚫ | ⚫ | 🟡 |
| Visual Basic | 18 | `vb` | ✅ | ✅ | ✅ |
| Visual Prolog | 84 | `visualprolog` | ⚫ | ⚫ | 🟡 |
| XML | 9 | `xml` | ✅ | ✅ | ✅ |
| YAML | 49 | `yaml` | ⚫ | ⚫ | 🟡 |

**Lisp (2026-07-02):** uses Lexilla's `lisp` lexer
(`LexLisp.cxx`) — a compact 12-slot byte-exact case-sensitive
S-expression lexer with a state-7 gap in the public style range
(`SciLexer.h:676-677` jumps `SCE_LISP_STRING=6` directly to
`SCE_LISP_STRINGEOL=8` — there is no `SCE_LISP_CHARACTER`, unlike
Bash / Lua / Perl / Python). Two-class wordlist surface —
`LISP_KEYWORDS` (class 0, functions and special operators) and
`LISP_KEYWORDS_KW` (class 1, `&`-prefixed lambda-list markers like
`&rest`, `&key`, `&optional`). Nine-mapping `LISP_STYLES` covers
COMMENT + MULTI_COMMENT → Comment italic (both `;`-line and
`#|...|#` block forms), NUMBER, KEYWORD → Keyword bold (class-0
hit), KEYWORD_KW → Keyword2 (class-1 hit — `&`-marker steel blue),
SYMBOL → Lifetime (`:kw` and `'quoted` sigil-tagged symbols — Bash
SCALAR / PARAM precedent), STRING, OPERATOR, SPECIAL → Keyword bold
(earmuffed globals `*foo*` / `+bar+` plus `#'foo` / `#\c` / `#xFF`
reader-macro emissions — structural-anchor archetype matching TCL
EXPAND). DEFAULT (0), IDENTIFIER (9), STRINGEOL (8) intentionally
unmapped per the universal-omission + deferred-Error pattern;
STRINGEOL is additionally never emitted at runtime (grep of
`LexLisp.cxx` returns zero hits for the constant).

The `.cxx`-private state markers 29 / 30 / 31
(`SCE_LISP_CHARACTER` / `MACRO` / `MACRO_DISPATCH`) `#define`d at
`LexLisp.cxx:32-34` are transient parse states — never emitted as
final styles, and deliberately NOT exported from `scintilla-sys`.
The `lisp` lexer also drives the `L_SCHEME` row via the same
shared-lexer pattern that PHP / HTML / ASP use with `hypertext` —
see the Scheme rationale below for `SCHEME_THEME`, which rides
this same `LISP_STYLES` table with a distinct `SCHEME_KEYWORDS` /
`SCHEME_KEYWORDS_KW` pair.

Authored by a 4-agent research-and-synthesise workflow. Structural
guards pinned in `lisp_uses_lexlisp_two_class_theme`: byte-exact
lowercase invariant on both wordlists, `&`-prefix contract on
`LISP_KEYWORDS_KW` (parallels `NSIS_VARIABLES`'s `$`-prefix guard),
`:`-symbol unreachable-token guard (`:kw` symbols enter
`SCE_LISP_SYMBOL` via `LexLisp.cxx:107-109` and never reach
`classifyWordLisp` — `:`-prefixed wordlist entries would be spec
noise), `HashSet` cross-class no-overlap guard, canonical-anchor
pins (`defun` in class 0, `&rest` in class 1), and 10 cross-language
non-reuse pins.

**Scheme (2026-07-02):** rides the same Lexilla `lisp` lexer as
L_LISP — `SCHEME_THEME` in `ui_win32/src/lib.rs` reuses `LISP_STYLES`
/ `LISP_ITALIC` / `LISP_BOLD` by direct `&'static` reference (pinned
by `std::ptr::eq` in
`scheme_reuses_lexlisp_theme_with_r7rs_wordlists`). Only the
wordlists differ: `SCHEME_KEYWORDS` (class 0, ~245 tokens) carries
R7RS §7 formal syntax (`define`, `define-syntax`, `letrec`, `begin`,
`lambda`, `syntax-rules`, `call/cc`, `dynamic-wind`, …) plus the
§6.1–6.14 procedure canon (bytevectors, R7RS I/O `read-line` /
`write-string` / `peek-u8`, character/string maps, library forms
`define-library` / `import` / `export`) plus SRFI-1 higher-order
idioms (`filter`, `fold`, `fold-left`, `fold-right`, `reduce`) which
are de facto standard even in R7RS-small codebases;
`SCHEME_KEYWORDS_KW` (class 1, ~80 tokens) carries the R7RS
predicate/mutator vocabulary — every entry ends in `?` (predicate:
`null?`, `pair?`, `eqv?`, `char-alphabetic?`, `string<=?`, …) or `!`
(mutator: `set-car!`, `vector-set!`, `bytevector-copy!`). The
class-1 `?`/`!` sigil contract is Scheme's semantic parallel to
Lisp's syntactic leading-`&` contract; both hit
`SCE_LISP_KEYWORD_KW` → Keyword2 in the shared table.

Tokens that moved: the four-line "// Scheme forms coexist" block in
`LISP_KEYWORDS` (which had mixed R7RS `define` / `null?` /
`set-car!` etc. into CL's wordlist) was migrated wholesale into
`SCHEME_KEYWORDS` / `SCHEME_KEYWORDS_KW`. Correctness gain: CL now
lists only `defun` / `labels` / `rplaca` / `atom` / `null` / `eq` /
`equal` (no `?`, no `!`); Scheme now lists only the R7RS canon.
`set!` lives in Scheme class 0 as an R7RS §4.1.6 assignment
special form, NOT in class 1 with the `!`-suffix data mutators —
a guard pin enforces this.

Extension coverage extended from `.scm` / `.ss` to include R7RS
`.sld` (define-library file, canonical in Chibi / Chicken /
Sagittarius / Gauche) and R6RS `.sls` (library source, deployed
in Chez / Guile / Larceny). `.rkt` deliberately not claimed —
Racket has diverged from R7RS (`#lang racket`, `struct`,
`require`/`provide`, `match`, `for/list` are not R7RS); a future
`L_RACKET` row is the right destination. `.sps` (R6RS program
script) skipped as too rare. `.smd` (Notepad++ ships) skipped —
not a standard Scheme extension in any implementation.

This is the first wiring of the "shared Lexilla lexer, distinct
wordlists" pattern applied to a two-class lexer — precedent from
`HYPERTEXT_STYLES` across HTML / PHP / ASP / XML (single-class
installs) applied to `LexLisp`. Reusable template for JSON5
(riding `json`) and future shared-lexer siblings: copy the
`LangTheme` struct; keep styles/italic/bold pointing at the shared
`&'static` slice; swap only the `keywords: &[…]` pair. Structural
guards pinned in `scheme_reuses_lexlisp_theme_with_r7rs_wordlists`:
`std::ptr::eq` on styles/italic/bold (catches copy-paste
divergence), two-class install shape, `HashSet` cross-class
no-overlap, byte-exact lowercase invariant, `?`/`!` sigil contract
on class 1, `set!` class-0 placement guard, `:`-prefix
unreachable-token guard, canonical anchors (`define` in class 0,
`null?` in class 1), and 10 cross-language WORDLIST non-reuse pins
(content-based, NOT style-based, because Scheme intentionally
shares styles with Lisp).

**Assembly (2026-07-03):** uses Lexilla's `asm` lexer (`LexAsm.cxx`,
`SCLEX_ASM`) covering x86-family (16 / 32 / 64-bit) sources across
MASM / NASM / GAS dialects — the eight-class
`asmWordListDesc[]` at `LexAsm.cxx:80-90` is filled across six
populated classes plus the two empty fold-only tail. `ASM_CPU_KEYWORDS`
(class 0, ~300 mnemonics) is the primary scalar-integer /
control-flow archetype (`mov`, `add`, `jmp`, `call`, `ret`, `push`,
`pop`, string ops, set-on-condition, system, cache management);
`ASM_FPU_KEYWORDS` (class 1, ~95 x87 mnemonics) covers the classic
ST(0)-based FPU (`fld`, `fadd`, `fsin`, `fisttp`, `fwait`, …);
`ASM_REG_KEYWORDS` (class 2, ~240 registers) enumerates every
architecturally-visible x86-64 register across all widths — general
(8/16/32/64-bit `al` through `r15`), instruction pointer, flags,
segment, control, debug, FPU stack, MMX, SSE/AVX/AVX-512 vector
(`xmm0..zmm31`), AVX-512 mask (`k0..k7`), and MPX bound
(`bnd0..bnd3`); `ASM_DIRECTIVE_KEYWORDS` (class 3, ~260 entries) is
the MASM ∪ NASM ∪ GAS union — MASM `proc` / `endp` / `.data` /
`invoke`, NASM `%macro` / `%define` / `section` / `resb` / `db`,
GAS `.text` / `.globl` / `.type` / `.cfi_*`; `ASM_DIRECTIVE_OP_KEYWORDS`
(class 4, ~35 qualifiers) carries size specifiers (`byte`, `dword`,
`xmmword`, `zmmword`), distance modifiers (`ptr`, `near`, `far`,
`offset`), and MASM segment attributes; `ASM_EXT_KEYWORDS` (class 5,
~495 mnemonics) is the SIMD family — MMX, SSE1–4.2, AES-NI,
PCLMULQDQ, SHA extensions, AVX (VEX-encoded), FMA3, AVX-512
(F/CD/DQ/BW + masking), and 3DNow!.

Six theme routings paint distinctly: `SCE_ASM_CPUINSTRUCTION` →
`StyleSlot::Keyword` (bold blue — primary archetype);
`SCE_ASM_MATHINSTRUCTION` → `Keyword2` (x87 secondary);
`SCE_ASM_REGISTER` → `Lifetime` slot (distinctive hue — visual
scanning of `rax` / `xmm7` / `k3` at a glance is the primary way
assembly readers track data flow, matches LISP_SYMBOL /
Bash SCALAR "sigil-tagged archetype" precedent);
`SCE_ASM_DIRECTIVE` → `Preprocessor` (assembler pseudo-ops read
as "out-of-band syntax markers", same slot as C `#include`);
`SCE_ASM_DIRECTIVEOPERAND` → `Keyword2`;
`SCE_ASM_EXTINSTRUCTION` → `Macro` (SIMD needs to pop out in
vectorised inner loops — RUST_MACRO precedent for
"special-flavor instruction"). Comment family (`COMMENT` +
`COMMENTBLOCK` + `COMMENTDIRECTIVE`) all italic; `CPUINSTRUCTION`
single-entry bold (RUST_BOLD precedent).

Case handling: `LexAsm` calls `GetCurrentLowered(s, sizeof(s))`
at `:332` before every `InList` check — wordlists ship
lowercase-only. Uppercase entries never match (assembler source
`MOV` / `mov` / `Mov` all become `mov` before the classifier
runs). Structural guards pinned in `asm_uses_lexasm_six_class_theme`:
14-mapping style table, six-class install shape, `HashSet`
cross-class + intra-class no-overlap (LexAsm's first-match-wins
chain at `:335-347` demotes duplicates), lowercase-only contract,
canonical anchors (`mov` / `fld` / `rax` / `.text` / `ptr` /
`vmovss` across the six classes), `"comment"` MUST-be-in-class-3
for MASM `COMMENT ~...~` block-comment lexing, 14 style-routing
pins, three deliberate-omission pins (`DEFAULT` / `IDENTIFIER` /
`STRINGEOL`), italic and bold set-shape pins, and 10 cross-language
non-reuse pins (unique REGISTER-as-Lifetime + EXT-as-Macro slot
picks).

Deferred: classes 6/7 (`Directives4Foldstart` / `Directives4Foldend`)
are consulted only by the folder at `LexAsm.cxx:490-500` and left
empty — a future commit can populate them with matched pairs
(`proc`/`endp`, `%macro`/`%endmacro`, `.if`/`.endif`) to enable
directive-pair folding without disturbing the classifier chain.

**Diff (2026-07-03):** uses Lexilla's `diff` lexer (`LexDiff.cxx`,
`SCLEX_DIFF`) — the smallest lexer family in Lexilla. There is no
tokeniser and no wordlist: `ColouriseDiffLine` at
`LexDiff.cxx:38-101` inspects the leading character(s) of each line
via strict-case `strncmp` chains at `:43-89` and paints the entire
line with a single style, so every `SCE_DIFF_*` index corresponds
to one **line archetype**. `emptyWordListDesc[]` at `:149-151`
formalises the no-wordlist contract; `DIFF_THEME.keywords` is
correspondingly the first row in the framework with an empty
`&[]` — no `SCI_SETKEYWORDS` calls issue.

Eleven theme routings across six palette slots preserve the
visual contract that added lines read GREEN and removed lines
read RED: `SCE_DIFF_ADDED` and `SCE_DIFF_PATCH_ADD` share
`StyleSlot::Comment` (green) with `SCE_DIFF_COMMENT` — the shared
colour value is intentional, and the added-line indices are
deliberately excluded from `DIFF_ITALIC` so only the preamble
prose ("Only in ...", "Binary file ...") tilts; `SCE_DIFF_DELETED`,
`SCE_DIFF_PATCH_DELETE`, `SCE_DIFF_REMOVED_PATCH_ADD`, and
`SCE_DIFF_REMOVED_PATCH_DELETE` all share `StyleSlot::String`
(brick red — the palette's red slot); `SCE_DIFF_CHANGED` →
`StyleSlot::Lifetime` (amber — a third distinct colour for
context-diff `!` lines that are neither strictly added nor
removed); `SCE_DIFF_COMMAND` → `StyleSlot::Keyword` (blue bold
— top-of-diff `diff ...` / `Index: ...` anchor);
`SCE_DIFF_HEADER` → `StyleSlot::Preprocessor` (purple — file
boundaries read as out-of-band syntax markers); `SCE_DIFF_POSITION`
→ `StyleSlot::Number` (magenta — hunk headers dominated by
numeric line ranges like `@@ -12,7 +34,8 @@`). `SCE_DIFF_DEFAULT`
stays unmapped so unchanged context lines keep `STYLE_DEFAULT`
and recede visually.

Structural guards pinned in `diff_uses_lexdiff_line_shape_theme`:
zero-keyword install (LexDiff ignores wordlists), 11-mapping
style table (12 slots minus DEFAULT), eight cross-language
non-reuse pins, 11 style-routing pins, semantic colour
contract (ADDED / PATCH_ADD share green; DELETED / PATCH_DELETE
/ REMOVED_PATCH_ADD / REMOVED_PATCH_DELETE share red),
DEFAULT-unmapped guard, single-entry italic set
(`SCE_DIFF_COMMENT` only — added / removed / changed content
stays upright for fast review scanning), single-entry bold set
(`SCE_DIFF_COMMAND` only — `RUST_BOLD` / `ASM_BOLD` precedent).

**PostScript (2026-07-03):** uses Lexilla's `ps` lexer
(`LexPS.cxx`, `SCLEX_PS`) covering Adobe PostScript's
stack-based token grammar across 16 style classes and 5
wordlist classes. The classifier at `LexPS.cxx:67-270` runs a
per-character state machine over `SCE_C_DEFAULT` neutral state,
entering typed states on self-delimiting punctuation (`[` `]`
`{` `}` `/` `<` `>` `(` `)` `%`) or on the leading char of a
number / identifier / string. `PS_LEVEL1_KEYWORDS` (class 0,
215 tokens) covers the core stack / math / array / dictionary /
string / boolean / control / type / file / graphics-state /
CTM / path / painting / font vocabulary shipped in every
PostScript interpreter since Level 1 (1985);
`PS_LEVEL2_KEYWORDS` (class 1, 83 tokens) adds device-
independent colour spaces (setters plus the family
discriminators `DeviceGray` / `CIEBasedA` / `Indexed` /
`Pattern` / `Separation`), patterns / forms, resources,
page-device parameters, object serialisation, per-context
graphics-state objects with local/global-VM management
(`setglobal` / `currentglobal`), the halftone-type
discriminator (`HalftoneType`), character positioning
variants (including `glyphshow`), user-path operators, and
the Level 2 filter mechanism (`ASCII85Decode` / `DCTDecode`
/ `LZWDecode` / `RunLengthDecode` / `SubFileDecode` /
`NullEncode` plus the Encode counterparts);
`PS_LEVEL3_KEYWORDS` (class 2, 9 tokens) is deliberately
minimal — only the genuine Level 3 additions:
smooth shading (`shfill` / `setsmoothness` /
`currentsmoothness`), idiom recognition
(`setidiomrecognition` / `currentidiomrecognition`), the
one Level 3 colour-space addition (`DeviceN`), and the
Level 3 stream filters (`FlateDecode` / `FlateEncode` /
`ReusableStreamDecode`). The three level lists were
scrubbed for classification drift during code review —
several colour-space discriminators, filter names, VM
operators, and `HalftoneType` were initially mis-placed
in Level 3 (works accidentally at the default
`ps.level = 3`, but silently hides those operators when a
user sets `ps.level = 1` or `2`); they've been moved to
Level 2 where PLR §3.7.2 / §3.13 / §4.5.6 / §4.8 / §7.4
place them. Classes 3 (RIP-specific) and 4 (user-defined)
are downstream-extension points and are omitted from the
install — LexPS's `:156-159` classifier chain safely
queries default-constructed empty `WordList`s via `InList`
when the host skips `SCI_SETKEYWORDS`, verified against
`LexerBase.cxx:32-34` (allocates `KEYWORDSET_MAX+1` empty
`WordList`s at construction) and `WordList.cxx:154-156`
(returns `false` immediately on null `words` pointer). Same
parking pattern as ASM's fold-only classes 6/7.

Not on the wordlist (by design): dictionary keys like
`ShadingType`, `FunctionType`, `ShadingDict` are always
written with a leading `/` in PostScript source, so LexPS
tokenises them as `SCE_PS_LITERAL` at `LexPS.cxx:208` — a
state that terminates at `:164-166` without ever consulting
the wordlist chain. Adding them to a `PS_LEVEL*` list would
be inert regardless of which level. Real Level 3 operator
identifiers (`shfill`, `FlateDecode`, …) are used bare,
without a `/`, and reach the classifier at `:152-163`.

Thirteen theme routings paint the buffer distinctly:
`SCE_PS_KEYWORD` → `StyleSlot::Keyword` (bold blue — primary
operator archetype); `SCE_PS_LITERAL` → `Keyword2` (steel blue
— `/name` literal-name literals are symbol references, closest
match to the secondary keyword slot); `SCE_PS_IMMEVAL` → `Macro`
(violet — `//name` immediately-evaluated names are a distinct
Level-2 concept, differentiated from plain LITERAL by the
`Macro` slot the same way Rust's `println!` is set apart from
regular identifiers); `SCE_PS_DSC_COMMENT` → `Preprocessor`
(purple italic — `%%directive` DSC lines are structural
file-level metadata, semantically parallel to C's `#define` /
`#include`); `SCE_PS_DSC_VALUE` → `String` (the actual textual
payload after the `:` in a DSC directive); PostScript's three
paren-family states (`PAREN_ARRAY` for `[` `]`, `PAREN_DICT` for
`<<` `>>`, `PAREN_PROC` for `{` `}`) all map to `Operator`
(dark grey) — the underlying constructs remain differentiable by
shape at a glance, colour doesn't need to over-differentiate;
the three string archetypes (`TEXT` for `(...)`, `HEXSTRING` for
`<...>`, `BASE85STRING` for `<~...~>`) all share `String` (brick
red). `SCE_PS_COMMENT` italic; `SCE_PS_KEYWORD` bold (single-entry
`RUST_BOLD` / `ASM_BOLD` / `DIFF_BOLD` precedent). `SCE_PS_DEFAULT`
/ `SCE_PS_NAME` / `SCE_PS_BADSTRINGCHAR` intentionally unmapped —
neutral state, unmatched-identifier, and error-marker
respectively (same convention as `SCE_ASM_STRINGEOL` /
`SCE_LISP_STRINGEOL`).

Case handling: `LexPS` calls `sc.GetCurrent(s, sizeof(s))` at
`LexPS.cxx:155`, **NOT** `GetCurrentLowered` — wordlist matching
is case-sensitive. PostScript is a case-sensitive language;
canonical mixed-case identifiers like `FontDirectory`,
`StandardEncoding`, `ISOLatin1Encoding`, `HalftoneType`, and
filter names (`ASCII85Decode`, `DCTDecode`, `FlateDecode`, …)
appear with their exact case in the wordlists. Contrast with
`LexAsm` (case-folded via `GetCurrentLowered`) and `LexLisp`
(case-sensitive but conventionally lowercase-only). Structural
guards pinned in `ps_uses_lexps_three_level_theme`: 13-mapping
style table, three-class install shape (with the RIP + user-defined
downstream-extension parking explicitly documented), `HashSet`
cross-class no-overlap (guards Level 2 / 3 entries against being
shadowed by Level 1 duplicates via the `||` short-circuit at
`:156-159`), **case-sensitive contract** (each of Level 1 / 2 / 3
must contain at least one canonical mixed-case identifier —
signals wordlist author didn't confuse LexPS with a case-folded
lexer), seven canonical anchors across the three levels (`add` +
`moveto` + `FontDirectory` for Level 1, `setcolorspace` +
`ISOLatin1Encoding` for Level 2, `shfill` + `FlateDecode` for
Level 3), 13 style-routing pins, three deliberate-omission pins
(`DEFAULT` / `NAME` / `BADSTRINGCHAR`), italic set == `COMMENT` +
`DSC_COMMENT`, bold set == `KEYWORD` only, 10 cross-language
non-reuse pins (unique `LITERAL`-as-`Keyword2` +
`IMMEVAL`-as-`Macro` slot picks).

Deferred: `SCE_PS_PAREN_ARRAY` / `PAREN_DICT` / `PAREN_PROC` all
share `StyleSlot::Operator`. A future palette redesign could
differentiate procedure braces (`{` `}`) from array brackets
(`[` `]`) and dict markers (`<<` `>>`) for stronger structural
visual cues, but the current single-slot picks match the
palette's coarse-by-design convention (`StyleSlot` doc says "reuse
what fits semantically" rather than "invent a new slot per
language"). Classes 3 (RIP) and 4 (user-defined) sit empty
pending downstream demand for printer-driver-specific or
site-macro operator sets.

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
