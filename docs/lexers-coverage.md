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

Total: 89 rows. ✅ 20 / 🟡 68 / ⚫ 1.

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
| Assembly | 32 | `asm` | ⚫ | ⚫ | 🟡 |
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
| LaTeX | 74 | `latex` | ⚫ | ⚫ | 🟡 |
| Lisp | 30 | `lisp` | ⚫ | ⚫ | 🟡 |
| Lua | 23 | `lua` | ⚫ | ⚫ | 🟡 |
| Makefile | 10 | `makefile` | ✅ | ✅ | ✅ |
| Matlab | 44 | `matlab` | ⚫ | ⚫ | 🟡 |
| MMIXAL | 75 | `mmixal` | ⚫ | ⚫ | 🟡 |
| Nim | 76 | `nim` | ⚫ | ⚫ | 🟡 |
| Nncrontab | 77 | `nncrontab` | ⚫ | ⚫ | 🟡 |
| NSIS | 28 | `nsis` | ⚫ | ⚫ | 🟡 |
| Objective-C | 5 | `cpp` | ✅ | ✅ | ✅ |
| OScript | 78 | `oscript` | ⚫ | ⚫ | 🟡 |
| Pascal | 11 | `pascal` | ✅ | ✅ | ✅ |
| Perl | 21 | `perl` | ✅ | ✅ | ✅ |
| PHP | 1 | `hypertext` | ✅ | ✅ | ✅ |
| PostScript | 35 | `ps` | ⚫ | ⚫ | 🟡 |
| PowerShell | 53 | `powershell` | ⚫ | ⚫ | 🟡 |
| Properties | 34 | `props` | — | ✅ | ✅ |
| Purebasic | 68 | `purebasic` | ⚫ | ⚫ | 🟡 |
| Python | 22 | `python` | ⚫ | ⚫ | 🟡 |
| R | 54 | `r` | ⚫ | ⚫ | 🟡 |
| Raku | 89 | `raku` | ⚫ | ⚫ | 🟡 |
| REBOL | 79 | `rebol` | ⚫ | ⚫ | 🟡 |
| Registry | 80 | `registry` | ⚫ | ⚫ | 🟡 |
| Resource file | 7 | `cpp` | ✅ | ✅ | ✅ |
| Ruby | 36 | `ruby` | ⚫ | ⚫ | 🟡 |
| Rust | 81 | `rust` | ✅ | ✅ | ✅ |
| S-Record | 61 | `srec` | ⚫ | ⚫ | 🟡 |
| SAS | 91 | `sas` | ⚫ | ⚫ | 🟡 |
| Scheme | 31 | `lisp` | ⚫ | ⚫ | 🟡 |
| Shell | 26 | `bash` | ⚫ | ⚫ | 🟡 |
| Smalltalk | 37 | `smalltalk` | ⚫ | ⚫ | 🟡 |
| Spice | 82 | `spice` | ⚫ | ⚫ | 🟡 |
| SQL | 17 | `sql` | ✅ | ✅ | ✅ |
| Swift | 64 | `cpp` | ⚫ | ⚫ | 🟡 |
| TCL | 29 | `tcl` | ⚫ | ⚫ | 🟡 |
| Tektronix extended HEX | 63 | `tehex` | ⚫ | ⚫ | 🟡 |
| TeX | 24 | `tex` | ⚫ | ⚫ | 🟡 |
| TOML | 90 | `toml` | ⚫ | ⚫ | 🟡 |
| txt2tags | 83 | `txt2tags` | ⚫ | ⚫ | 🟡 |
| TypeScript | 85 | `cpp` | ⚫ | ⚫ | 🟡 |
| Verilog | 43 | `verilog` | ⚫ | ⚫ | 🟡 |
| VHDL | 38 | `vhdl` | ⚫ | ⚫ | 🟡 |
| Visual Basic | 18 | `vb` | ✅ | ✅ | ✅ |
| Visual Prolog | 84 | `visualprolog` | ⚫ | ⚫ | 🟡 |
| XML | 9 | `xml` | ✅ | ✅ | ✅ |
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
