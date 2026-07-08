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
| — (Keywords column only) | Not applicable. The lexer takes no wordlists at all — host installs none by design. Currently used for `props` (INI / Properties — a pure line-prefix classifier that ignores its `WordList *[]` parameter), `registry` (Windows Registry files — a state-machine lexer whose `WordListSet` unconditionally returns -1, actively REJECTING any keyword install), and `txt2tags` (structural markup — the `LexerModule` registration takes no `wordListDesc` argument at all and the paint function's `WordList **` parameter is unnamed and never referenced). A row with `—` in the Keywords column and ✅ in the Theme column is still ✅ overall: the wiring is complete, there are simply no keywords to wire. |
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

Total: 89 rows. ✅ 75 / 🟡 13 / ⚫ 1.

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
covers JSP. `L_JAVASCRIPT` itself (the standalone `.js` /
`.mjs` / `.cjs` LexCPP-family row) landed separately —
that wired only the `.js`-file lexer path; the
hypertext-embedded `<script>` block classifier remains as
tracked on HTML / PHP.

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
Embedded `<script>` JavaScript and `<style>` CSS deferred to a
follow-up: the `.js`-file `L_JAVASCRIPT` row landed but
`HTML_THEME` still doesn't install `JAVASCRIPT_KEYWORDS` /
`CSS_*` into the hypertext-embedded-script classes (class 1 /
class 2 of `htmlWordListDesc`). Same scope discipline as
PHP's `SCE_HJ_*` / `SCE_HB_*` deferral. Authored by a 7-agent
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
| Ada | 42 | `ada` | ✅ | ✅ | ✅ |
| ASN.1 | 65 | `asn1` | ⚫ | ⚫ | 🟡 |
| ASP | 16 | `hypertext` | ✅ | ✅ | ✅ |
| Assembly | 32 | `asm` | ✅ | ✅ | ✅ |
| AutoIt | 40 | `au3` | ✅ | ✅ | ✅ |
| AviSynth | 66 | `avs` | ⚫ | ⚫ | 🟡 |
| BaanC | 60 | `baan` | ⚫ | ⚫ | 🟡 |
| Batch | 12 | `batch` | ✅ | ✅ | ✅ |
| Blitzbasic | 67 | `blitzbasic` | ⚫ | ⚫ | 🟡 |
| C | 2 | `cpp` | ✅ | ✅ | ✅ |
| C# | 4 | `cpp` | ✅ | ✅ | ✅ |
| C++ | 3 | `cpp` | ✅ | ✅ | ✅ |
| Caml | 41 | `caml` | ✅ | ✅ | ✅ |
| CMake | 48 | `cmake` | ✅ | ✅ | ✅ |
| COBOL | 50 | `COBOL` | ✅ | ✅ | ✅ |
| CoffeeScript | 56 | `coffeescript` | ✅ | ✅ | ✅ |
| CSound | 70 | `csound` | ✅ | ✅ | ✅ |
| CSS | 20 | `css` | ✅ | ✅ | ✅ |
| D | 52 | `d` | ✅ | ✅ | ✅ |
| Diff | 33 | `diff` | ✅ | ✅ | ✅ |
| Erlang | 71 | `erlang` | ✅ | ✅ | ✅ |
| ErrorList | 92 | `errorlist` | ⚫ | ⚫ | 🟡 |
| ESCRIPT | 72 | `escript` | ✅ | ✅ | ✅ |
| Forth | 73 | `forth` | ✅ | ✅ | ✅ |
| Fortran (fixed form) | 59 | `f77` | ✅ | ✅ | ✅ |
| Fortran (free form) | 25 | `fortran` | ✅ | ✅ | ✅ |
| Freebasic | 69 | `freebasic` | ⚫ | ⚫ | 🟡 |
| GDScript | 86 | `gdscript` | ✅ | ✅ | ✅ |
| Go | 88 | `cpp` | ✅ | ✅ | ✅ |
| Gui4Cli | 51 | `gui4cli` | ✅ | ✅ | ✅ |
| Haskell | 45 | `haskell` | ✅ | ✅ | ✅ |
| Hollywood | 87 | `hollywood` | ✅ | ✅ | ✅ |
| HTML | 8 | `hypertext` | ✅ | ✅ | ✅ |
| INI file | 13 | `props` | — | ✅ | ✅ |
| Inno Setup | 46 | `inno` | ✅ | ✅ | ✅ |
| Intel HEX | 62 | `ihex` | ⚫ | ⚫ | 🟡 |
| Java | 6 | `cpp` | ✅ | ✅ | ✅ |
| Javascript | 58 | `cpp` | ✅ | ✅ | ✅ |
| JSON | 57 | `json` | ✅ | ✅ | ✅ |
| JSON5 | 94 | `json` | ✅ | ✅ | ✅ |
| JSP | 55 | `hypertext` | ✅ | ✅ | ✅ |
| KIXtart | 39 | `kix` | ✅ | ✅ | ✅ |
| LaTeX | 74 | `latex` | ✅ | ✅ | ✅ |
| Lisp | 30 | `lisp` | ✅ | ✅ | ✅ |
| Lua | 23 | `lua` | ✅ | ✅ | ✅ |
| Makefile | 10 | `makefile` | ✅ | ✅ | ✅ |
| Matlab | 44 | `matlab` | ✅ | ✅ | ✅ |
| MMIXAL | 75 | `mmixal` | ✅ | ✅ | ✅ |
| Nim | 76 | `nim` | ✅ | ✅ | ✅ |
| Nncrontab | 77 | `nncrontab` | ✅ | ✅ | ✅ |
| NSIS | 28 | `nsis` | ✅ | ✅ | ✅ |
| Objective-C | 5 | `cpp` | ✅ | ✅ | ✅ |
| OScript | 78 | `oscript` | ✅ | ✅ | ✅ |
| Pascal | 11 | `pascal` | ✅ | ✅ | ✅ |
| Perl | 21 | `perl` | ✅ | ✅ | ✅ |
| PHP | 1 | `hypertext` | ✅ | ✅ | ✅ |
| PostScript | 35 | `ps` | ✅ | ✅ | ✅ |
| PowerShell | 53 | `powershell` | ✅ | ✅ | ✅ |
| Properties | 34 | `props` | — | ✅ | ✅ |
| Purebasic | 68 | `purebasic` | ⚫ | ⚫ | 🟡 |
| Python | 22 | `python` | ✅ | ✅ | ✅ |
| R | 54 | `r` | ✅ | ✅ | ✅ |
| Raku | 89 | `raku` | ✅ | ✅ | ✅ |
| REBOL | 79 | `rebol` | ✅ | ✅ | ✅ |
| Registry | 80 | `registry` | — | ✅ | ✅ |
| Resource file | 7 | `cpp` | ✅ | ✅ | ✅ |
| Ruby | 36 | `ruby` | ✅ | ✅ | ✅ |
| Rust | 81 | `rust` | ✅ | ✅ | ✅ |
| S-Record | 61 | `srec` | ⚫ | ⚫ | 🟡 |
| SAS | 91 | `sas` | ⚫ | ⚫ | 🟡 |
| Scheme | 31 | `lisp` | ✅ | ✅ | ✅ |
| Shell | 26 | `bash` | ✅ | ✅ | ✅ |
| Smalltalk | 37 | `smalltalk` | ✅ | ✅ | ✅ |
| Spice | 82 | `spice` | ✅ | ✅ | ✅ |
| SQL | 17 | `sql` | ✅ | ✅ | ✅ |
| Swift | 64 | `cpp` | ⚫ | ⚫ | 🟡 |
| TCL | 29 | `tcl` | ✅ | ✅ | ✅ |
| Tektronix extended HEX | 63 | `tehex` | ⚫ | ⚫ | 🟡 |
| TeX | 24 | `tex` | ✅ | ✅ | ✅ |
| TOML | 90 | `toml` | ⚫ | ⚫ | 🟡 |
| txt2tags | 83 | `txt2tags` | — | ✅ | ✅ |
| TypeScript | 85 | `cpp` | ✅ | ✅ | ✅ |
| Verilog | 43 | `verilog` | ✅ | ✅ | ✅ |
| VHDL | 38 | `vhdl` | ✅ | ✅ | ✅ |
| Visual Basic | 18 | `vb` | ✅ | ✅ | ✅ |
| Visual Prolog | 84 | `visualprolog` | ✅ | ✅ | ✅ |
| XML | 9 | `xml` | ✅ | ✅ | ✅ |
| YAML | 49 | `yaml` | ✅ | ✅ | ✅ |

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

**Ruby (2026-07-03):** uses Lexilla's `ruby` lexer
(`LexRuby.cxx`, `SCLEX_RUBY`) — the largest single-file lexer
in Lexilla at 2191 lines. The classifier state machine at
`LexRuby.cxx:1043-1770` distinguishes context-sensitive uses
of `if` / `do` / `while` / `unless` / `until` / `for` (leader
vs trailing modifier via `keywordIsModifier` at `:1803`),
infers identifier category from sigil (`$` global, `@`
instance, `@@` class, `:` symbol) and position (post-`class`
/ `module` / `def` → the identifier being defined), and
admits trailing `?` / `!` on identifiers at `:1418-1425`
(so `defined?` in `RUBY_KEYWORDS` matches the tokenised
segment). One wordlist class — "Keywords" per
`rubyWordListDesc[]` at `:142-145` — carries 41 Ruby 3.x
reserved words including canonical uppercase (`BEGIN` /
`END`) and double-underscore magic constants (`__FILE__` /
`__LINE__` / `__ENCODING__`). Kernel methods like `raise`,
`throw`, `catch`, `loop`, `require`, `__method__` and the
`attr_*` family are intentionally EXCLUDED — they're
ordinary method calls, not reserved words. Case-sensitive
matching via `styler.GetRange` at `:335-337` (NOT
`GetCurrentLowered`) — the uppercase / mixed-case entries
must appear with their exact case in the wordlist.

Thirty-four theme routings across 37 emissible slots (32 at
indices 0..=31 + 5 at 40..=44; indices 32..=39 are a sub-style
reservation range for `SCE_RB_IDENTIFIER` per `:156, :211`,
never emitted directly), minus 3 deliberate omissions
(`DEFAULT` / `ERROR` / `IDENTIFIER`). `SCE_RB_UPPER_BOUND`
(45) is a pseudo-style `#define` for
`SCE_RB_IDENTIFIER_PREFERRE` at `:333` that only ever sets
the classifier's internal `preferRE` flag at `:1440-1444` —
never reaches the host and so isn't counted as emissible.
Ruby's 16-way string family (4 direct — `STRING`,
`CHARACTER`, `BACKTICKS`, `REGEX`; 5 percent-literal variants
— `%q`, `%Q`, `%x`, `%r`, `%W`; 3 non-interp variants — `%w`,
`%i`, `%I`; 4 heredoc variants — delimiter + `_Q` + `_QQ` +
`_QX`) all consolidate onto `StyleSlot::String`
— the palette's brick-red slot carries the semantics
regardless of Ruby's percent-literal / heredoc syntax
variations. The sigil-tagged scoped bindings are the
theme's most-differentiated area: `INSTANCE_VAR` (`@foo`)
and `CLASS_VAR` (`@@foo`) share `StyleSlot::Lifetime`
(amber — the slot doc-comment says "reuse for similar
scoped-binding highlights"; `@` / `@@` sigils are exactly
that), while `GLOBAL` (`$foo`, `$0`..`$9`, `$_`, `$~`,
`$&`, …) routes to `StyleSlot::Macro` (violet — distinct
sigil class). `SYMBOL` (`:foo`) and the built-in stream
constants `STDIN` / `STDOUT` / `STDERR` share
`StyleSlot::Preprocessor` (purple — out-of-band syntax
markers / distinct namespace). Definition names
(`CLASSNAME` post-`class`, `DEFNAME` post-`def`,
`MODULE_NAME` post-`module`) share `StyleSlot::Keyword2`
(steel-blue). `WORD` and `WORD_DEMOTED` (the
trailing-modifier usage of an ambiguous keyword like
`stmt if cond`) both route to `StyleSlot::Keyword` for a
matched blue colour, but only `WORD` is bold — the weight
change flags the modifier role while keeping the
language-level identity clear. Comment family (`COMMENTLINE`
+ `POD` + `DATASECTION`) all share the Comment green,
with `COMMENTLINE` + `POD` italic; `DATASECTION` (post-
`__END__` payload) shares the colour but stays upright,
since a long data block would visually dominate if
italicised.

Structural guards pinned in `ruby_uses_lexruby_theme`:
34-mapping style table, single-class install shape,
case-sensitive contract (at least one uppercase / mixed-case
canonical entry), 9 canonical anchors (`def` / `class` /
`if` / `end` / `nil` / `self` / `defined?` / `BEGIN` /
`__FILE__`), 34 style-routing pins, 4 deliberate-omission
pins (`DEFAULT` / `ERROR` / `IDENTIFIER` / `UPPER_BOUND`),
string-family bucket assertion (16 string archetypes all
route to `StyleSlot::String`), sigil-tagged binding
category (`INSTANCE_VAR` + `CLASS_VAR` share `Lifetime`,
`GLOBAL` gets `Macro` — distinct sigil classes must not
collide), symbol / stdio bucket (5 indices share
`Preprocessor`), italic set == `COMMENTLINE` + `POD`,
bold set == `WORD` only (single-entry
primary-class-bold; `RUST_BOLD` / `ASM_BOLD` / `DIFF_BOLD`
/ `PS_BOLD` precedent), 10 cross-language non-reuse pins.

Excluded from `RUBY_KEYWORDS` by design: Kernel methods
(`puts`, `print`, `warn`, `eval`) — LexRuby paints those
via a separate special-case at `:393-395` that promotes
them to the pseudo-style `SCE_RB_IDENTIFIER_PREFERRE`
regardless of wordlist membership, so listing them would
be redundant and could interfere with the demotion
detection at `:359-360`. Constants (`STDIN` / `STDOUT` /
`STDERR`) — LexRuby directly emits these via dedicated
`SCE_RB_STDIN` / `STDOUT` / `STDERR` slots (30 / 31 / 40)
so wordlist entry would be inert. The IDENTIFIER
sub-styling range (indices 32..=39, reserved for host-
allocated sub-styles via `SCI_ALLOCATESUBSTYLES`) is left
untouched — a future commit can allocate sub-styles for
built-in Ruby type discrimination (`Array` / `Hash` /
`String` etc.) via `SubStyles` at `:211`.

**Smalltalk (2026-07-03):** uses Lexilla's `smalltalk` lexer
(`LexSmalltalk.cxx`, `SCLEX_SMALLTALK`) — a compact 330-line
character-class-dispatch classifier for a language where
"everything is a message send." Auto-generated
`ClassificationTable[256]` at `LexSmalltalk.cxx:71-80`
categorises source chars into five sets (`DecDigit`, `Letter`,
`Special`, `Upper`, `BinSel`); the driver at `:272-322`
dispatches to typed handlers (`handleHash` for `#symbol`,
`handleSpecial` for `()[]{};.^:` punctuation, `handleNumeric`
for radix-supporting numerics, `handleLetter` for
identifier + keyword-send + hardcoded-word disambiguation,
`handleBinSel` for `~@%&*-+=|\/,<>?!` binary selectors).
Case-sensitive matching via byte-exact `strcmp` at `:257-266`
for the five hardcoded language constants (`self` / `super`
/ `nil` / `true` / `false`), and via
`wordLists[0]->InList` at `:250` for the single
"Special selectors" wordlist class.

Sixteen theme routings across 17 `SCE_ST_*` slots (minus
`DEFAULT` — universal neutral-state skip). Smalltalk's
distinctive feature is that its language-keyword archetype
is split across SIX dedicated SCE indices — the hardcoded
constants (`SELF` / `SUPER` / `NIL` / `BOOL`), the return
operator `^` (`RETURN`), and the wordlist-matched
control-flow selectors (`SPEC_SEL`) — all six route to
`StyleSlot::Keyword` (bold blue). This is a deliberate
divergence from every other wired lexer's single-entry bold
precedent (`RUST_BOLD` = `SCE_RUST_WORD` only; `ASM_BOLD` =
`SCE_ASM_CPUINSTRUCTION` only; `RB_BOLD` = `SCE_RB_WORD`
only): where LexRuby / LexRust use a single `SCE_*_WORD`
slot and rely on wordlist matching to differentiate keyword
subclasses, LexSmalltalk pre-splits its keyword archetype at
the classifier level. To preserve the "one visual weight
for language keywords" contract, all six dedicated slots
must be bolded together — bolding only one would leave
`nil` bold but `true` non-bold (or vice versa), which reads
as visually incoherent.

`GLOBAL` (10) — UpperCase-first identifiers, Smalltalk
convention for class names and globals — routes to
`Keyword2` (steel blue). `KWSEND` (13) — keyword-send
message parts ending in `:` that did NOT match the wordlist
(`at:`, `put:`, `do:`, `collect:`, `printOn:`) — also
`Keyword2`, keeping ordinary sends visually prominent
without over-bolding. `SYMBOL` (4) — `#foo` symbol literals
— routes to `Preprocessor` (purple, matching Ruby's `:foo`
convention). String literals (`STRING` for `'...'`,
`CHARACTER` for `$c`) share `StyleSlot::String` (brick red).
The three operator classes (`BINARY` — user-defined binary
selectors, `SPECIAL` — punctuation, `ASSIGN` — `:=`) share
`StyleSlot::Operator` (dark grey).

`SMALLTALK_SPECIAL_SELECTORS` carries 15 canonical
control-flow / nil-test / boolean-combinator selectors —
SciTE's default 11-entry set from
`vendor/lexilla/test/examples/smalltalk/SciTE.properties`
(`ifTrue:` `ifFalse:` `whileTrue:` `whileFalse:` `ifNil:`
`ifNotNil:` `whileTrue` `whileFalse` `repeat` `isNil`
`notNil`) plus the 4 short-circuit boolean combinators
(`and:` `or:` `xor:` `not`). Ordinary utility sends
(`at:`, `put:`, `do:`, `collect:`, `printString`,
`printOn:`) are DELIBERATELY excluded — over-bolding
ordinary message sends defeats the visual control-flow
signal, and those sends already paint distinctly as
`SCE_ST_KWSEND` (steel-blue Keyword2). Compound selectors
(`ifTrue:ifFalse:`, `to:by:do:`) are also excluded because
`handleLetter` at `:241-247` admits only ONE trailing `:`
per identifier segment — compounds tokenise as separate
atoms that each need their own single-part wordlist entry.
The five hardcoded constants (`self` / `super` / `nil` /
`true` / `false`) are excluded for the OPPOSITE reason
one might first assume: `handleLetter`'s dispatch order
at `:250-266` puts `InList` FIRST (line 250) with the
hardcoded strcmp chain as the last-chance fallback
(`:257-266`). So adding those five to the wordlist would
silently promote them to `SCE_ST_SPEC_SEL` and OVERRIDE
their dedicated `SCE_ST_SELF` / `SUPER` / `NIL` / `BOOL`
styles — the opposite of intended visual differentiation.
The exclusion enforces that `InList` doesn't win a
precedence it shouldn't win.

Structural guards pinned in `smalltalk_uses_lexsmalltalk_theme`:
16-mapping style table, single-class install shape,
15-token wordlist-count pin (guards against ordinary-send
leakage), 10 canonical anchors across control-flow /
nil-test / boolean combinators, 3 exclusion bucket pins
(no hardcoded constants, no compounds, no utility sends —
5 + 4 + 6 = 15 forbidden-token checks total), 16
style-routing pins, `DEFAULT`-unmapped guard, single-entry
italic set (`COMMENT` only), 6-entry bold set with
non-bold sanity checks on `GLOBAL` / `KWSEND` / `BINARY`
/ `SPECIAL` / `ASSIGN` / `SYMBOL`, 10 cross-language
non-reuse pins.

**VHDL (2026-07-03):** uses Lexilla's `vhdl` lexer
(`LexVHDL.cxx`, ~600 lines including folder) — a case-insensitive
IEEE-1076 hardware-description lexer with the widest
wordlist-class fan-out we've wired (7 classes vs 1-3 for most
lexers). The classifier's `SCE_VHDL_IDENTIFIER` state is an
intermediate stop: on scan exit at `LexVHDL.cxx:90-107`,
`GetCurrentLowered` case-folds the identifier and probes each
of `keywordlists[0..6]` sequentially, promoting to KEYWORD /
STDOPERATOR / ATTRIBUTE / STDFUNCTION / STDPACKAGE / STDTYPE /
USERWORD respectively. The identifier scan state never survives
to paint time when a wordlist matches — so `VHDL_STYLES`
deliberately leaves both DEFAULT (0) and IDENTIFIER (6) unmapped.
16 SCE_VHDL_* slots minus those two = 14 style mappings.

The 7 wordlist classes populated per `VHDLWordLists[]` at
`:552-561` (case-insensitive contract per §13.4 — every wordlist
entry MUST be lowercase because `GetCurrentLowered` case-folds
before `InList`):

- **`VHDL_KEYWORDS` (class 0, 82 tokens):** IEEE-1076-1993
  reserved words plus `protected` (VHDL-2002+). VHDL-2008
  additions (`assume`, `context`, `cover`, `default`,
  `fairness`, `force`, `parameter`, `property`, `release`,
  `restrict`, `sequence`, `strong`, `vunit`) intentionally
  excluded pending broader VHDL-2008 coverage — the fold
  routine doesn't fold on them either, so adding here without
  matching folder work would create inconsistency.
- **`VHDL_OPERATORS` (class 1, 16 tokens):** the exact
  IEEE-1076 §7.2 word-form operator set (`abs`, `and`, `mod`,
  `nand`, `nor`, `not`, `or`, `rem`, `rol`, `ror`, `sla`,
  `sll`, `sra`, `srl`, `xnor`, `xor`). Distinct SCE style
  (`SCE_VHDL_STDOPERATOR`) so the theme can bold word
  operators AS keywords — which is how a VHDL author reads
  them.
- **`VHDL_ATTRIBUTES` (class 2, 31 tokens):** IEEE-1076
  §14.1 predefined attributes (`left`, `right`, `low`,
  `high`, `range`, `length`, `event`, `stable`, etc.). Note
  `range` overlaps `VHDL_KEYWORDS` — the classifier probes
  class 0 FIRST at `:93` so the Attributes-list entry for
  `range` is dead, but kept for parity with the upstream
  banner at `:578-581`.
- **`VHDL_STDFUNCTIONS` (class 3, 28 tokens):** functions
  from `std.textio`, `ieee.std_logic_1164`, and
  `ieee.numeric_std` / `numeric_bit`. Note the upstream
  banner writes `to_UX01` (mixed case) but the wordlist
  MUST use `to_ux01` since the lexer case-folds before
  match.
- **`VHDL_STDPACKAGES` (class 4, 17 tokens):** the fixed
  libraries (`std`, `ieee`, `work`) plus package names
  under `std` (`standard`, `textio`) and `ieee`
  (`std_logic_1164`, `numeric_std`, `math_real`, VITAL, etc.).
- **`VHDL_STDTYPES` (class 5, 28 tokens):** predefined types
  from `std.standard` (`boolean`, `bit`, `integer`, `real`,
  etc.), `std.textio` (`line`, `text`, `side`, `width`),
  `ieee.std_logic_1164` (`std_ulogic`, `std_ulogic_vector`,
  `std_logic`, `std_logic_vector`, plus the four
  case-folded logic-value subtypes `x01`, `x01z`, `ux01`,
  `ux01z` — again, lowercase because the lexer case-folds),
  and `ieee.numeric_std` (`unsigned`, `signed`).
- **`VHDL_USERWORDS` (class 6, empty):** deliberately empty
  — the per-project extension slot the lexer author designed
  for user-populated identifiers via a project-level
  override. Reserved as the default-empty value the future
  per-project override layers on top of. Empty install is
  required (not skippable) because `LexerBase.h:19` + `.cxx:32-34`
  pre-allocate `KEYWORDSET_MAX+1 = 9` `WordList*` slots and the
  classifier addresses slot 6 unconditionally at `:105`.

The theme choices: KEYWORD + STDOPERATOR paint the same
bold-keyword weight (both read as language reserved words);
ATTRIBUTE gets the Preprocessor purple accent (matches Ruby
`:symbol` / Smalltalk `#symbol` — the "designator-that-lives-
after-a-sigil" family); STDPACKAGE gets the Macro accent
(package names read as top-level namespaces akin to C
`#define` targets); STDFUNCTION and USERWORD share the
Keyword2 teal (library helper + user-flagged identifier);
STDTYPE reuses the Number-tinted accent for type-family
tokens. OPERATOR paints as Operator, comments as Comment
(all three of COMMENT / COMMENTLINEBANG / BLOCK_COMMENT are
italicised), strings as String (STRING and STRINGEOL both
mapped so the runaway-string error indicator stays visible).

Only KEYWORD and STDOPERATOR are bold — not ATTRIBUTE /
STDFUNCTION / STDPACKAGE / STDTYPE / USERWORD, which get
their identity from colour rather than weight, matching the
"one bold visual for language keywords" rule used across the
framework.

Structural test coverage: 12 invariants including
7-class-in-correct-order pin (class-index dispatch precedence
at `:93-107`), 5-class-non-empty guard + explicit
User-Words-empty guard, per-wordlist all-lowercase pin
(guards against re-copying the upstream banner's mixed-case
`to_UX01` / `X01` / `X01Z` etc.), 14 style-routing pins,
DEFAULT + IDENTIFIER unmapped guards, 3-entry italic set
(comment family), 2-entry bold set (KEYWORD + STDOPERATOR),
6 cross-language non-reuse pins, 5 anchor reserved-word pins
(`entity`, `architecture`, `signal`, `begin`, `end`), and a
`X01`-family case-folding guard that asserts NONE of the
upstream banner's mixed-case forms slipped in.

**KIXtart (2026-07-04):** uses Lexilla's `kix` lexer
(`LexKix.cxx`, ~130 lines) — a compact classifier for KIXtart
4.x, a Windows login-script language (mid-abandoned in 2018 but
still deployed at legacy shops). The SCE numbering is
**non-contiguous** — 11 emission slots at 0..=10 plus
`SCE_KIX_IDENTIFIER` at 31 (a 20-index gap the author reserved
for future style additions). Case-insensitive language;
`GetCurrentLowered` at `LexKix.cxx:84` (macro path) and `:98`
(identifier path) case-fold before every wordlist probe, so
wordlist entries MUST be lowercase.

Three active wordlist classes drive three visually distinct
promotion paths:

- **`KIX_KEYWORDS` (class 0, 51 tokens):** KIXtart **commands**
  — statement-heading tokens ONLY: control flow (`if`,
  `else`, `while`, `for`, `next`, `function`, `endfunction`,
  `gosub`, `return`, `select`, `case`), variable declarations
  (`dim`, `redim`, `global`), filesystem statements (`use`,
  `copy`, `move`, `del`, `md`, `rd`, `cd`, `run`, `shell`),
  console I/O statements (`cls`, `color`, `at`, `get`, `gets`,
  `beep`, `sleep`), and time (`settime`, `include`). Class 0
  fires FIRST at `LexKix.cxx:100-101` — matches promote from
  `SCE_KIX_IDENTIFIER` to `SCE_KIX_KEYWORD`. **Deliberately
  excludes** `?` / `??` (LexKix's classifier can't reach the
  identifier-exit path for them — `IsAWordChar` at `:33-35`
  and `IsOperator` at `:37-39` both reject `?`, so the state
  machine never enters `SCE_KIX_IDENTIFIER` on `?`) and the
  registry / printer / config command-forms (`addkey`,
  `delkey`, `writevalue`, `delvalue`, `addprinterconnection`,
  `logevent`, `settitle`, `setconsole`, `setl`, `setm`,
  `setascii`, `setoption`, `setwallpaper`, `setfileattr`) —
  all documented as FUNCTIONS in the KIXtart 4.x reference,
  idiomatically called in expression context. They live in
  `KIX_FUNCTIONS`; duplicating them here would silently mask
  the FUNCTIONS entry because `LexKix.cxx:100-103` probes
  class 0 first.
- **`KIX_FUNCTIONS` (class 1, 105 tokens):** KIXtart
  **built-in functions** — expression-usable helpers with
  return values: string utilities (`left`, `right`, `substr`,
  `len`, `instr`, `lcase`, `ucase`, `trim`, `replace`),
  numeric coercions (`cint`, `cstr`, `cdbl`, `val`,
  `dectohex`), filesystem queries (`dir`, `fileexists`,
  `getfileattr`, `getfilesize`, `open`, `close`, `readline`,
  `writeline`), registry (`readvalue`, `writevalue`,
  `delvalue`, `addkey`, `delkey`, `enumkey`, `enumvalue`,
  `existkey`, `loadhive`, `unloadhive`, `savekey`, `savedkey`,
  `readtype`), printer connection (`addprinterconnection`,
  `delprinterconnection`, `setdefaultprinter`), event log
  (`logevent`, `backupeventlog`, `cleareventlog`), system
  config (`settitle`, `setconsole`, `setl`, `setm`,
  `setascii`, `setoption`, `setwallpaper`, `setfileattr`),
  object interop (`createobject`, `getobject`), and system
  introspection (`memorysize`, `getdiskspace`, `messagebox`,
  `sendkeys`). Class 1 fires after class 0 at `:102-103` —
  matches promote from `SCE_KIX_IDENTIFIER` to
  `SCE_KIX_FUNCTIONS`. The commands-vs-functions split is
  important — a KIXtart author reads them as visually
  distinct categories, and a token misused (e.g., `use` on
  an expression RHS) is almost always a bug the theme
  should surface.
- **`KIX_MACROS` (class 2, 76 tokens):** KIXtart
  **built-in macros** — the fixed vocabulary of `@`-prefixed
  runtime constants. Names WITHOUT the leading `@`
  (classifier strips the sigil via `&s[1]` at `:86` before
  probing). Covers identity (`userid`, `username`,
  `fullname`, `wksta`), time (`date`, `time`, `day`,
  `month`, `year`, `ticks`, `msecs`), network (`address`,
  `hostname`, `ipaddress0`-`ipaddress3`, `ldrive`,
  `ldomain`), system info (`cpu`, `mhz`, `build`, `csd`,
  `dos`, `kix`, `syslang`, `pid`, `ras`), and script
  metadata (`scriptdir`, `scriptexe`, `scriptname`,
  `curdir`, `cwd`, `result`, `serror`). **This class is a
  whitelist gate**, not a promotion path: MACRO state is
  entered at `:121-122` on the `@` sigil, and at scan exit
  the whitelist decides whether MACRO STAYS (matched) or
  DOWNGRADES to DEFAULT (unmatched). So a typo like
  `@daat` paints as default text, not as a false-positive
  macro — the whole point of the whitelist.

A fourth class (`keywords4`) is declared but commented out at
`LexKix.cxx:47` — Code++ doesn't install it. The three
sigil-prefixed style families (`$var` → `SCE_KIX_VAR`,
`@macro` → `SCE_KIX_MACRO`) include the sigil in the emitted
style run (consistent with Perl / Bash / Ruby sigil-family
convention). String semantics are identical for both quote
forms (`"..."` STRING1 and `'...'` STRING2) — no escape
sequences in either.

Theme choices:
- KEYWORD → `Keyword` (bold-blue); FUNCTIONS → `Keyword2`
  (teal) — the two-tone contrast makes commands-vs-functions
  visually obvious.
- VAR (`$sigil`) → `Lifetime` — matches Rust's `'lt`
  convention for sigil-prefixed identifiers.
- MACRO (`@sigil`) → `Preprocessor` — the purple accent
  matches Ruby's `:symbol` and Smalltalk's `#symbol`
  (designator-follows-sigil family). Distinct from VAR so
  the two sigil families are visually distinguishable at a
  glance.
- STRING1 + STRING2 → `String` (both quote forms share
  identical semantics); NUMBER → `Number`; OPERATOR →
  `Operator`; COMMENT (`;`) + COMMENTSTREAM (`/* */`) →
  `Comment` (both italic).
- Bold only on KEYWORD (KIXtart commands read as reserved
  words). FUNCTIONS gets its identity from colour, matching
  the framework's "one bold visual for language keywords"
  rule.

Structural test coverage: 14 invariants including
3-class-in-canonical-order pin, per-wordlist lowercase guard,
`@`-sigil-not-in-KIX_MACROS guard (protects against the
mistake of listing macro entries as `@date` instead of
`date`), 10 style-routing pins, DEFAULT + IDENTIFIER unmapped
guards (IDENTIFIER at slot 31 is the intermediate state for
unknown user-defined identifiers — those paint as default
text, not as false-positive keywords), 6 cross-language
non-reuse pins, and 5+5+5 anchor pins across commands /
functions / macros (`if`, `else`, `while`, `for`, `next`,
`function`, `endfunction`; `getobject`, `createobject`,
`messagebox`, `left`, `right`; `date`, `time`, `userid`,
`wksta`, `scriptdir`).

**AutoIt (2026-07-04):** uses Lexilla's `au3` lexer
(`LexAU3.cxx`, ~910 lines including folder) — a rich lexer for
AutoIt3, a Windows automation / scripting language. The **widest
wordlist-class fan-out** we've wired: eight named classes at
`LexAU3.cxx:900-909` — keywords, functions, macros, SendKeys,
preprocessors, special, expand, UDF. Case-insensitive; `tolower`
at `:247` case-folds before every wordlist probe, so entries
MUST be lowercase.

The classifier's unique features:

- **SendKeys tokens matched INSIDE strings.** Every other AutoIt
  wordlist matches at the identifier boundary in ordinary source;
  SendKeys are matched inside `Send("...")` / `ControlSend(...)`
  string arguments. The classifier's `SCE_AU3_STRING` state at
  `:437-461` peeks for `{`/`+`/`!`/`^`/`#` and transitions into
  `SCE_AU3_SENT`, then on the closing `}` runs `GetSendKey`
  (`:106-169`) to produce the brace-wrapped token `sk` and
  probes `keywords4.InList(sk)` at `:483-486`. So
  `Send("{ENTER}")` paints as STRING—SENT—STRING with `{ENTER}`
  distinctly coloured.
- **Two sigil families, opposite convention from KIXtart.** The
  `$var` sigil enters `SCE_AU3_VARIABLE` at `:550` and the `$`
  is INCLUDED in the emitted style run. The `@macro` sigil
  enters the SCE_AU3_KEYWORD scan state at `:552` and — unlike
  KIXtart's LexKix which strips the `@` via `&s[1]` before
  probing — LexAU3 keeps the `@` in the identifier that reaches
  `InList`. So `AU3_MACROS` entries MUST have the leading `@`.
  Same for `AU3_PREPROCESSORS` and the `#` sigil at `:549`.
- **COM object member access.** `$obj.Method` paints as
  `$obj` VARIABLE, `.` OPERATOR, `Method` COMOBJ (via the
  transition at `:299-302` when `sc.chPrev == '.'` and next
  char is a word char). Distinct SCE style so method calls on
  COM objects paint differently from bare identifiers.

Eight wordlist classes populated per `AU3WordLists[]`:

- **`AU3_KEYWORDS` (class 0, 44 tokens):** AutoIt3 reserved
  words — control flow (`if` / `else` / `elseif` / `endif` /
  `while` / `wend` / `for` / `to` / `step` / `next` / `select`
  / `case` / `endselect` / `switch` / `endswitch` / `do` /
  `until` / `with` / `endwith`), function control (`func` /
  `endfunc` / `return` / `exit` / `exitloop` / `continueloop`
  / `continuecase`), variable declarations (`dim` / `local` /
  `global` / `const` / `enum` / `redim` / `static` / `byref` /
  `volatile` / `readonly`), constants (`true` / `false` /
  `null` / `default`), and word operators (`and` / `or` /
  `not`).
- **`AU3_FUNCTIONS` (class 1, 370 tokens):** AutoIt3 built-in
  functions — a representative subset of the ~1200 total
  built-in surface (one of the largest in Windows scripting).
  Covers strings, GUI create + control (~50 `GUICtrl*`
  functions alone), filesystem, registry, process control,
  windows, controls, math, arrays, mouse, clipboard, timers,
  networking (TCP/UDP/HTTP), DLL calls, COM interop, and
  environment. Extension slot for a future per-project override
  covers the residual surface.
- **`AU3_MACROS` (class 2, 101 tokens):** `@`-prefixed runtime
  macros. Path (`@ScriptDir`, `@TempDir`, `@WindowsDir`),
  identity (`@ComputerName`, `@UserName`), time (`@YEAR` /
  `@MON` / `@MDAY` / `@HOUR` / `@MIN` / `@SEC`), display
  (`@DesktopWidth` / `@DesktopHeight`), OS
  (`@OSVersion` / `@OSArch`), error state (`@error` /
  `@extended` / `@exitcode`), and constants (`@CR` / `@LF` /
  `@CRLF` / `@TAB`, plus the `@SW_*` window-state constants
  used with `Run()` / `WinSetState()`).
- **`AU3_SENDKEYS` (class 3, 91 tokens):** brace-wrapped
  SendKeys tokens (`{ENTER}`, `{TAB}`, `{F1}`-`{F12}`, arrow
  keys, numpad, lock keys, modifier tokens, browser keys,
  volume, media, launch keys, plus the literal-punctuation
  escapes `{!}` / `{#}` / `{+}` / `{^}` / `{\{}` / `{\}}`).
- **`AU3_PREPROCESSORS` (class 4, 34 tokens):** `#`-prefixed
  compiler / preprocessor directives (`#include` /
  `#include-once` / `#region` / `#endregion` /
  `#notrayicon` / `#requireadmin` / `#pragma`, plus the full
  `#autoit3wrapper_*` metadata set that AutoIt3Wrapper
  understands). **Deliberately excludes** `#cs` /
  `#comments-start` / `#ce` / `#comments-end` — those are
  handled by dedicated literal-string branches at `:320-324`
  and `:260-264` that promote directly to
  `SCE_AU3_COMMENTBLOCK` before the wordlist probe would ever
  fire.
- **`AU3_SPECIAL` (class 5, empty):** project-extension slot.
  Empty install required — the classifier addresses class 5
  unconditionally at `:346`.
- **`AU3_EXPAND` (class 6, empty):** project-extension slot for
  multi-line expand constructs. The bare `_` line-continuation
  is intentionally NOT here — dedicated hard-coded path at
  `:358-360` promotes it to OPERATOR.
- **`AU3_UDF` (class 7, 90 tokens):** AutoIt3 Standard UDF
  Library (the underscore-prefixed helpers shipped in
  `Include/*.au3` with the AutoIt3 compiler). Covers arrays
  (`_Array*`), date/time (`_Date*`), file (`_File*`), GUI
  (`_GUICtrl*`), math (`_Math_*`), string (`_String*`),
  Windows API (`_WinAPI_*`), inet, and misc. Under 100 tokens
  — representative subset of the ~600-1000 UDF surface.
  Distinct SCE style so authors can visually distinguish
  first-party built-ins (FUNCTION) from UDF-library helpers.

Theme choices: KEYWORD → `Keyword` (bold-blue); FUNCTION / UDF
/ COMOBJ → `Keyword2` (teal — three "callable helpers" the
language provides). SPECIAL also routes to `Keyword2` but for a
different reason — the class ships empty as a per-project
extension slot, and the teal accent reserves a distinctive visual
signal for whatever a future per-project override populates it
with; the "callable helper" grouping doesn't apply. MACRO / SENT
→ `Preprocessor`
(purple accent for the two structured-named-token families —
`@Error` runtime macros AND `{ENTER}` SendKeys share the visual
"this is a named token, not string content" role); PREPROCESSOR
→ `Macro` (a distinct accent slot — the framework's
Rust-macro accent, deliberately not `Preprocessor` so
AutoIt3's `#include` paints differently from the `@Error`
runtime macros, which use `Preprocessor`); VARIABLE (`$name`)
→ `Lifetime` (matches
Rust `'lt`, KIXtart `$var`, Ruby `SCE_RB_INSTANCE_VAR`); EXPAND
→ `Keyword` (empty class today; visual slot reserved for a
future per-project extension); COMMENT (`;`) + COMMENTBLOCK
(`#cs ... #ce`) → `Comment` (both italic); STRING → `String`;
NUMBER → `Number`; OPERATOR → `Operator`.

Only KEYWORD is bolded. FUNCTION / UDF / COMOBJ share the
Keyword2 colour accent but not weight, matching the framework's
"one bold visual for language keywords" rule.

Structural test coverage: 14 invariants including
8-class-in-canonical-order pin, non-empty guards for classes 0
/ 1 / 2 / 3 / 4 / 7, empty guards for classes 5 / 6, per-wordlist
lowercase pin (guards case-insensitive contract), `@`-sigil
required in `AU3_MACROS`, `#`-sigil required in
`AU3_PREPROCESSORS`, brace-wrapping required in `AU3_SENDKEYS`,
15 style-routing pins, `DEFAULT`-unmapped guard, 2-entry italic
set (comment family), 1-entry bold set (KEYWORD), 5
cross-language non-reuse pins, and 5+3+3+3+2 anchor pins across
keywords / functions / macros / sendkeys / preprocessors.

**Caml (2026-07-04):** uses Lexilla's `caml` lexer (`LexCaml.cxx`,
~330 lines) — a dual-mode OCaml / Standard ML '97 lexer
contributed by Robert Roessler (2005-2009). Case-sensitive
(opposite of AutoIt / VHDL / KIXtart), with a **runtime
SML-mode sentinel** at `LexCaml.cxx:71` — the classifier probes
`keywords.InList("andalso")` on entry and, if `andalso` is
present, switches every mode-dependent branch to Standard ML
rules (numeric literal syntax, char-literal `#"..."` form, tag
suppression, extra identifier chars). Code++ ships OCaml mode;
`CAML_KEYWORDS` deliberately OMITS `andalso`, and the test
invariant #5 pins that.

**Nested block comments.** OCaml supports arbitrarily-nested
`(* ... *)` block comments. LexCaml encodes nesting depth in the
SCE state by INCREMENTING the state on `(*` entry and
DECREMENTING on `*)` exit — SCE_CAML_COMMENT (12) → COMMENT1
(13) → COMMENT2 (14) → COMMENT3 (15). Depths beyond 3 are tracked
in a separate counter but reuse the COMMENT3 style. Code++ maps
all four to `StyleSlot::Comment` (uniform visual — depth doesn't
affect appearance, only the classifier's un-nesting bookkeeping).

**Three wordlist classes:**

- **`CAML_KEYWORDS` (class 0, 56 tokens):** primary OCaml
  reserved words — control flow (`if` / `then` / `else` /
  `match` / `when` / `for` / `to` / `downto` / `do` / `done`
  / `while` / `try`), value bindings (`let` / `rec` / `and`
  / `in` / `as` / `of`), function definition (`fun` /
  `function`), module system (`module` / `struct` / `sig` /
  `end` / `open` / `include` / `functor` / `with`), object
  system (`class` / `object` / `inherit` / `initializer` /
  `method` / `virtual` / `private` / `new` / `constraint`),
  type / exception / value declarations (`type` /
  `exception` / `val` / `external` / `mutable`), boolean
  literals (`true` / `false`), word-form operators (`or` /
  `lor` / `lxor` / `land` / `lsl` / `lsr` / `asr` / `mod` /
  `lazy`), and `assert` / `begin`. **Excludes `andalso`** —
  see SML-mode sentinel above.
- **`CAML_KEYWORDS2` (class 1, 97 tokens):** Pervasives /
  Stdlib functions. Since OCaml 4.07 (2018) `Pervasives` was
  renamed `Stdlib` but functions remain auto-opened at the
  top level. Covers I/O (`print_string` / `print_int` /
  `print_endline` / `read_line` / `open_in` / `input_line`
  / `output_string`), numeric conversion (`int_of_float`
  / `float_of_int` / `string_of_int` / `int_of_string`),
  combinators (`fst` / `snd` / `not` / `compare` / `min` /
  `max` / `abs` / `succ` / `pred` / `ignore`),
  Option / Result constructors (`Some` / `None` / `Ok` /
  `Error`), and error handling (`raise` / `failwith` /
  `invalid_arg` / `exit`).
- **`CAML_KEYWORDS3` (class 2, 41 tokens):** primitive and
  Stdlib type names. Built-ins (`int` / `float` / `string`
  / `bool` / `char` / `unit` / `bytes` / `int32` / `int64`
  / `nativeint` / `float32`), polymorphic containers (`list`
  / `array` / `option` / `result` / `seq` / `lazy_t` —
  `ref` is intentionally omitted, see below), exception +
  format (`exn` / `format` / `format4` / `format6`), and
  common capitalised stdlib module names as bare identifiers
  (`List` / `Array` / `String` / `Bytes` / `Hashtbl` /
  `Buffer` / `Printf` / `Scanf` / `Format` / etc.). Dot
  breaks the identifier in LexCaml, so `List.map` tokenises
  as `List` + `.` + `map` — the wordlist can only match
  bare parts, so the module portion appears here and the
  function name is picked up by class 1 if bare or nothing
  otherwise.

**Wordlist dispatch precedence at `LexCaml.cxx:141-146`**:
class 0 → class 1 → class 2. A token in both KEYWORDS2 and
KEYWORDS3 gets the KEYWORDS2 style — that's why `ref` (both a
function and a type constructor) is placed in `CAML_KEYWORDS2`
only, since the function reading is more common in surface
OCaml. The KEYWORDS3 entry would be dead code.

**Theme choices:** KEYWORD → `Keyword` (bold-blue); KEYWORD2
(functions) → `Keyword2` (teal); KEYWORD3 (types) → `Number`
(numeric-tinted accent — matches VHDL's STDTYPE convention).
TAGNAME (`` `Tag `` polymorphic-variant tags) → `Preprocessor`
(purple accent — the family's designator-follows-sigil style).
LINENUM (`#123` compile-time line markers) → `Macro` (the
framework's distinct-from-Preprocessor accent slot, shared with
`SCE_RB_GLOBAL` / `SCE_VHDL_STDPACKAGE` / `SCE_AU3_PREPROCESSOR`
— C's `SCE_C_PREPROCESSOR` actually routes to `Preprocessor`,
which TAGNAME already uses here). OPERATOR → `Operator`;
NUMBER → `Number`;
CHAR / STRING / WHITE → `String` (WHITE is the SML
embedded-whitespace escape, mapped defensively so a mis-flagged
`.sml` opened as `.ml` still renders sensibly). COMMENT +
COMMENT1/2/3 → `Comment` (all four italic).

**Only KEYWORD is bolded.** KEYWORD2 (functions) and KEYWORD3
(types) get their identity from colour rather than weight,
matching the framework's "one bold visual for language keywords"
rule.

Structural test coverage: 12 invariants including
`andalso`-must-be-absent guard (the CRITICAL SML-mode-sentinel
protection), 3-class order pin, all-non-empty guard, per-class
count assertion, 14 style-routing pins, DEFAULT + IDENTIFIER
unmapped guards (bare user identifiers paint as default text),
4-entry italic set (all comment nesting depths), 1-entry bold
set (KEYWORD only), 4 cross-language non-reuse pins, and
7+4+4 anchor pins across keywords / functions / types.

**Ada (2026-07-04):** uses Lexilla's `ada` lexer (`LexAda.cxx`,
~330 lines) — a single-wordlist case-insensitive lexer written
by Sergey Koshcheyev in 2002. Titled "Lexer for Ada 95" but
handles Ada 2005 / Ada 2012 syntax cleanly since none of those
revisions changed comment / string / numeric syntax — only the
reserved-word set grew (95 added `abstract` / `aliased` /
`protected` / `requeue` / `tagged` / `until`; 2005 added
`interface` / `overriding` / `synchronized`; 2012 added `some`).
`ADA_KEYWORDS` covers all four revisions — 73 tokens total, all
lowercase.

**Critical: lexer lowercases the source.** `LexAda.cxx:200-208`
folds every identifier byte via `tolower` before the
`keywords.InList` lookup — Ada language semantics: identifier
case does not distinguish tokens (`Package_Body` and
`PACKAGE_BODY` refer to the same declaration). Consequence: every
token in `ADA_KEYWORDS` MUST be lowercase; an uppercase or
mixed-case entry would be dead code (the lookup key is already
`begin` by the time InList runs). Ada Reference Manual convention
renders reserved words in bold lowercase, matching this. Test
invariant #5 pins every-token-lowercase and would flag any drift.

**Apostrophe disambiguation dependency.** Ada's `'` is overloaded:
`'a'` opens a character literal, but `X'Range` /
`Integer'First` opens an attribute selector. LexAda tracks the
per-line meaning with a `apostropheStartsAttribute` bool at
`LexAda.cxx:234-243` (persisted line-to-line via
`styler.SetLineState`). After any keyword hit the flag CLEARS
(next apostrophe = character literal) UNLESS the matched keyword
is exactly `all` (`:211-213`), because Ada pointer-dereference
syntax `Ptr.all'Address` is followed by attribute selection.
Consequence: `all` MUST remain in `ADA_KEYWORDS` — dropping it
would break character literals near dereference sites. Test
invariant #6 pins `all`-must-be-present specifically to catch
that failure mode.

**Style routing (10 mappings; `IDENTIFIER` and `DEFAULT`
deliberately unmapped):**

- **WORD (SCE_ADA_WORD, promoted from IDENTIFIER by wordlist
  hit)** → `Keyword` bold blue — the language's reserved words.
- **IDENTIFIER** — deliberately UNMAPPED. LexAda sets
  `SCE_ADA_IDENTIFIER` as the terminal state for every
  non-keyword word (variables, types, subprograms, packages);
  painting it would tint every bare name in an Ada source file
  with a palette accent. Every neighbor themed lexer (C, C++,
  Pascal, VHDL, KIXtart, Caml, AutoIt) omits its `_IDENTIFIER`
  slot for the same reason — the KIX banner in `ui_win32/src/lib.rs`
  documents this explicitly ("SHOULD paint as default text so the
  reader isn't visually assaulted by every function name"). Ada
  matches the convention.
- **NUMBER** → `Number`. Ada supports based literals (`16#FF#`,
  `2#1010#`) via LexAda's `#`-delimited base syntax.
- **DELIMITER** → `Operator`. Ada's multi-char operators
  (`:=`, `=>`, `..`, `**`, `<<`, `>>`, `<=`, `>=`, `/=`) each
  paint as consecutive single-char DELIMITER runs — LexAda
  doesn't fuse them, but the shared style makes them read as
  one span.
- **CHARACTER + CHARACTEREOL + STRING + STRINGEOL** → `String`.
  The two `*EOL` states (unterminated char / string literal)
  fall into `String` too — same lane as the well-formed
  variants, since the token content is still string-like even
  in error. Precedent: `VHDL_STYLES` maps `SCE_VHDL_STRINGEOL`
  to `String`. LexPas / LexHTML SGML / LexMake take the
  opposite path (unmapped, pending a future `StyleSlot::Error`).
  Ada picks the visible-in-lane choice; either convention is
  defensible.
- **LABEL (`<< target >>` goto targets)** → `Preprocessor`
  purple — "out-of-band syntax marker" reading fits the
  Preprocessor slot's visual semantics. Ada block labels are
  unusual in modern code so a distinct accent draws the eye.
- **COMMENTLINE (`--`)** → `Comment` green italic. Ada has no
  block comments; line comments are the sole comment form.
- **ILLEGAL** → `Macro`. A distinct high-contrast accent for
  tokens LexAda rejected (`IsValidIdentifier` fail or
  `IsValidNumber` fail). Borrowed from the `StyleSlot::Macro`
  slot (otherwise Rust-only) because Ada doesn't have real
  macros, and this gives syntax errors their own colour without
  adding a new palette slot.

Only the WORD style is bold — matching the framework's "one bold
visual for language keywords" rule.

**Single-class install.** `adaWordListDesc[]` at
`LexAda.cxx:42-45` declares one class only, `"Keywords"`, with
`adaWordListDesc[1] = 0` as the NULL sentinel. `ADA_THEME`
installs class 0 alone. Test invariant #3 pins the single-class
shape.

Structural test coverage: 12 invariants — 10 style mappings pin,
single-class-only pin, `ADA_KEYWORDS` non-empty guard,
every-token-lowercase pin (dead-code prevention),
`all`-must-be-present pin (apostrophe-disambiguation dependency),
10 style-routing pins, `DEFAULT` + `IDENTIFIER` unmapped pins
(bare user identifiers paint as default text), italic == 1
(`COMMENTLINE`), bold == 1 (`WORD` only), 5 cross-language
non-reuse pins (C++ / Ruby / VHDL / AutoIt / Caml), and 19 anchor
tokens across Ada 83 / 95 / 2005 / 2012 revisions.

**Verilog (2026-07-04):** uses Lexilla's `verilog` lexer
(`LexVerilog.cxx`, ~1080 lines) — a case-sensitive lexer written
by Avi Yegudin on top of Neil Hodgson's LexCPP frame and later
extended by Ted Fried with SystemVerilog states. Covers
Verilog-1995 / 2001 / 2005 (IEEE 1364) plus SystemVerilog
(IEEE 1800). File extensions `.v` / `.vh` / `.sv` / `.svh` all
route here.

**Six wordlist classes; three installed.** `verilogWordLists[]`
at `LexVerilog.cxx:1076-1084` declares six classes:

- **Class 0** (`VERILOG_KEYWORDS`, ~170 tokens): primary
  reserved words — module / interface / program / package /
  class structure (`module` / `endmodule` / `interface` /
  `class` / `extends`), procedural blocks (`always` /
  `always_comb` / `always_ff` / `always_latch` / `initial` /
  `final` / `fork` / `join`), control flow (`if` / `else` /
  `case` / `casex` / `casez` / `for` / `foreach` / `while`),
  continuous assignment (`assign` / `deassign` / `force` /
  `release`), timing (`posedge` / `negedge` / `wait` /
  `event`), generate (`generate` / `endgenerate` / `genvar`),
  and the SystemVerilog assertion / property / sequence
  temporal-operator family (`assert` / `assume` / `cover` /
  `property` / `sequence` / `always` / `eventually` /
  `nexttime` / `s_until` / `s_until_with` / `sync_accept_on`
  / `sync_reject_on` / `throughout` / `within` / …). Also
  covers coverage / constraint keywords (`covergroup` /
  `coverpoint` / `constraint` / `solve` / `dist` /
  `inside`).
- **Class 1** (`VERILOG_KEYWORDS_2`, ~75 tokens): types,
  net-types, gate primitives, and drive / charge strength
  qualifiers — the "shape and drive of signals" set,
  distinct from the control-flow keyword class. Variable
  types (`reg` / `integer` / `real` / `logic` / `bit` /
  `int` / `longint`), net-types (`wire` / `wand` / `wor` /
  `tri` / `supply0` / `supply1` / `trireg` / `uwire`),
  gate primitives (`and` / `or` / `xor` / `nand` / `nor` /
  `not` / `buf` / `nmos` / `pmos` / `cmos` / `tran` /
  `pullup` / `pulldown`), drive/charge strengths (`pull0` /
  `pull1` / `strong0` / `weak0` / `highz0` / `small` /
  `medium` / `large`), and SystemVerilog modifiers
  (`signed` / `unsigned` / `showcancelled` /
  `noshowcancelled` / `pulsestyle_ondetect` /
  `pulsestyle_onevent`).
- **Class 2** (`VERILOG_SYSTEM_TASKS`, ~180 tokens):
  `$`-prefixed system tasks and functions from IEEE 1364
  §17 and IEEE 1800 §20-25. Display / write family
  (`$display` / `$monitor` / `$strobe` / `$write` including
  the `b` / `o` / `h` radix variants), simulation control
  (`$finish` / `$stop` / `$time` / `$realtime`), file I/O
  (`$fopen` / `$fclose` / `$fdisplay` / `$fread` /
  `$sscanf` / `$readmemh` / `$readmemb` / `$writememh`),
  conversion (`$signed` / `$unsigned` / `$bitstoreal`),
  math (`$clog2` / `$ln` / `$log10` / `$exp` / `$sqrt` /
  `$pow` / `$sin` / `$cos` / `$tan` / `$atan2` /
  `$hypot`), randomization (`$random` / `$urandom` /
  `$urandom_range` / `$dist_normal` /
  `$dist_exponential` / `$dist_poisson`), assertion /
  severity (`$info` / `$warning` / `$error` / `$fatal` /
  `$assertoff` / `$asserton` / `$assertkill`), coverage
  (`$coverage_control` / `$get_coverage`), timing check
  system tasks (`$hold` / `$setup` / `$recovery` /
  `$removal` / `$skew` / `$period` / `$width` /
  `$nochange`), VCD dump family (`$dumpfile` / `$dumpvars`
  / `$dumpports` / `$dumpon` / `$dumpoff`), bit /
  vector introspection (`$bits` / `$high` / `$low` /
  `$isunknown` / `$countones` / `$onehot` / `$typename` /
  `$size`), and `$test$plusargs` / `$value$plusargs` for
  command-line argument parsing.
- **Class 3** (User-defined tasks / identifiers) — NOT
  installed. Ships empty; a future per-project override
  mechanism will populate this if the framework grows one.
  Present as an SCE state (`SCE_V_USER`) so mapping it in
  the theme is defensive: if the class is populated, the
  colour is already right.
- **Class 4** (Documentation comment keywords) — NOT
  installed. Doxygen-style keywords like `\author` /
  `\brief` / `\file` inside a block comment would fire
  `SCE_V_COMMENT_WORD` if this class were populated.
  Code++ ships without a canonical doc-keyword set for
  Verilog; a future doc-syntax pass could add one.
- **Class 5** (Preprocessor definitions) — NOT a
  highlighting class. `ppDefinitions` at
  `LexVerilog.cxx:317` populates the lexer's internal
  macro-expansion table for `` `define ``-style
  expansion during scanning. Not something a syntax theme
  installs.

**Case-sensitive lexer.** `LexVerilog.cxx:552-559` matches
wordlist entries byte-exactly — no `tolower` fold. All IEEE
1364 / 1800 reserved words are lowercase, so every entry stays
lowercase. Test invariant #6 pins every-token-lowercase (with
the leading `$` sigil skipped for the system-tasks class).

**System-task sigil.** `IsAWordStart` at `LexVerilog.cxx:362`
includes `$` as a word-start character, so an identifier
starting with `$` assembles into a single token including
the sigil. Consequence: `VERILOG_SYSTEM_TASKS` entries MUST
include the leading `$` — a bare `display` entry would be
unreachable (the InList probe key is always `$display`).
Test invariant #5 pins `$`-must-be-present on every
system-tasks entry.

**Style routing (17 mappings; `DEFAULT` and `IDENTIFIER`
unmapped):**

- **COMMENT + COMMENTLINE + COMMENTLINEBANG** → `Comment`
  green italic — all three comment forms share the
  italic-comment convention. `COMMENTLINEBANG` (`//!`) is a
  doc-flag variant that reads as a comment to the human eye
  but is emitted as a distinct SCE state so a themed skin
  could pop it visually if desired.
- **COMMENT_WORD** → `Preprocessor` purple. Doxygen-style
  keyword recognized inside a block comment; the "semantic
  marker inside prose" reading fits the Preprocessor slot's
  visual meaning.
- **NUMBER** → `Number`. Verilog's rich number syntax
  (sized `4'b1010` / `8'hFF` / `16'd42`, unsized decimals,
  real literals, underscore separators) all funnel here.
- **WORD (class 0)** → `Keyword` bold blue. Primary
  reserved words.
- **WORD2 (class 1)** → `Keyword2` teal. Types / net-types
  / gates / strengths get the secondary accent — they're
  reserved words too, but the visual hierarchy pins
  control-flow keywords as the primary emphasis.
- **WORD3 (class 2)** → `Macro`. Distinct accent for
  `$`-prefixed system tasks; the sigil already reads as
  out-of-band and reusing Keyword2 would blur the visual
  line between "language reserved word" and "runtime
  library call". Matches how PostScript's `SCE_PS_IMMEVAL`
  and Ruby's `SCE_RB_GLOBAL` borrow the Macro slot for the
  same "distinct sigil-marked identifier" reading.
- **USER (class 3)** → `Keyword2`. Same lane as WORD2
  since the semantic intent is "known library-ish
  identifier"; empty wordlist by default, so this only
  matters if a project override populates it.
- **PREPROCESSOR** (`` ` ``-directives) → `Preprocessor`.
  The canonical slot for out-of-band syntax markers.
- **OPERATOR** → `Operator`.
- **IDENTIFIER** — deliberately UNMAPPED. Framework
  convention (matches C / C++ / Pascal / VHDL / KIXtart /
  Caml / AutoIt / Ada).
- **STRING** → `String`.
- **STRINGEOL** → `String`. Same lane as STRING so the
  malformed region still reads as string-shaped; matches
  VHDL / Ada precedent.
- **INPUT / OUTPUT / INOUT** → `Keyword`. Matches the
  `SCE_V_WORD` baseline these tokens land in via class 0
  wordlist matching when `portStyling` is off (Code++'s
  default at `LexVerilog.cxx:146`), so toggling the option
  later doesn't create a visual jump on identical source
  characters (bold-blue Keyword either way).
- **PORT_CONNECT** → `Keyword2`. `.name` in `.name (expr)`
  module-instantiation binds reads as "known binding
  identifier" — same lane as USER (project-known helper
  names).

Only WORD is bold — matches the framework's "one bold visual
for language keywords" rule (Caml, VHDL, KIXtart, Ada all
follow this).

Structural test coverage: 12 invariants — 17 style mappings pin,
three-class order pin (classes 0 / 1 / 2 in canonical order),
all-non-empty guard for the three installed wordlists,
`$`-sigil-must-be-present on every system-tasks entry,
every-token-lowercase (with `$` stripped) across all three
classes, 17 style-routing pins, `DEFAULT` + `IDENTIFIER`
unmapped pins, italic == 4 (three comment styles + doc-word),
bold == 1 (`WORD` only), 4 cross-language non-reuse pins
(VHDL / Ada / C++ / Caml), and 47 anchor tokens spread across
Verilog-1995 core (15), SystemVerilog additions (12), types /
gates (12), and system tasks (8).

**MATLAB (2026-07-04):** uses Lexilla's `matlab` lexer
(`LexMatlab.cxx`, ~530 lines) — a case-sensitive lexer for
MATLAB and Octave, written by José Fonseca and extended over
the years by Christoph Dalitz (2003 — Octave support +
double-quoted strings), John Donoghue (2012-2017 — nested
block comments, `...` continuation-as-comment, fold refinements),
and Andrey Smolyakov (2022 — MATLAB R2019b+ `arguments` block
and `classdef` `properties` / `methods` / `events` contextual
keywords). Compact wiring: single-wordlist theme, 7 style
mappings, 21 reserved words (20 from MathWorks' `iskeyword`
plus `enumeration`, a classdef-body reserved word that
`iskeyword` does not return).

**Case-sensitive lexer.** `LexMatlab.cxx:251` calls
`keywords.InList(s)` byte-exactly — no `tolower` fold. All
MATLAB reserved words are lowercase per MathWorks' `iskeyword`
output, so every wordlist entry stays lowercase. Test invariant
#5 pins every-token-lowercase.

**Contextual keywords deliberately excluded from the wordlist.**
LexMatlab handles four MATLAB reserved-word tokens contextually
INSIDE the classifier rather than via the wordlist:

- `arguments` — promoted to `SCE_MATLAB_KEYWORD` only after a
  `function` declaration line (via the `expectingArgumentsBlock`
  flag at `LexMatlab.cxx:270-274`). The lexer's `:269` comment
  says outright "arguments is a keyword here, despite not being
  in the keywords list".
- `properties` / `methods` / `events` — promoted to
  `SCE_MATLAB_KEYWORD` only inside `classdef` scope (via the
  `inClassScope` flag and folding-level check at `:285-292`).
  Outside `classdef` they `ChangeState` to
  `SCE_MATLAB_IDENTIFIER` — so a user-declared variable named
  `properties` doesn't over-highlight.

Putting any of these four tokens in `MATLAB_KEYWORDS` would
break the lexer's deliberate contextual behaviour by promoting
them at every site. Test invariant #6 pins their absence so a
future edit that "helpfully" adds them regresses visibly. The
enumeration constant `enumeration` IS included — MathWorks
lists it as a reserved word and LexMatlab does not treat it
contextually.

**Context-sensitive `end`.** Inside indexing (i.e. when
`allow_end_op > 0` — the lexer's `(`/`[`/`{` counter),
`LexMatlab.cxx:255-257` ChangeState-s `end` to
`SCE_MATLAB_NUMBER`. That's the MATLAB idiom where `x(end)`
returns `x`'s last element — a "number-shaped" quantity, not a
control-flow keyword. The wordlist entry still needs `end`
present so InList fires and lets the classifier decide.

**Initial-state trick.** LexMatlab enters `SCE_MATLAB_KEYWORD`
as the INITIAL state for any alphabetic run at `:399-400`, then
either keeps it (InList hit at `:251`) or downgrades to
`SCE_MATLAB_IDENTIFIER` (InList miss at `:289`). This is the
reverse of most lexers (which start at IDENTIFIER and promote
to KEYWORD) — same visible outcome, opposite SCE-index
history. No impact on the theme wiring since we map both
states appropriately.

**Style routing (7 mappings; `DEFAULT` and `IDENTIFIER`
unmapped per framework convention):**

- **COMMENT** → `Comment` italic green. Covers all three
  MATLAB comment forms: `%` line comment, `%{ ... %}` nested
  block comment (depth tracked in line state), and `...`
  line-continuation which the classifier promotes to COMMENT
  at `:236`.
- **COMMAND** → `Preprocessor`. The `!command` shell-escape
  syntax reads as out-of-band; same slot the framework uses
  for `` `include ``-style markers elsewhere. Only fires
  under the MATLAB lexer (not Octave, which routes `!` to
  `SCE_MATLAB_OPERATOR` at `:387`).
- **NUMBER** → `Number`. Numeric literals plus contextual
  `end` inside indexing.
- **KEYWORD** → `Keyword` bold blue.
- **STRING** → `String`. Traditional single-quoted char-array
  literals. Contextual — the classifier disambiguates opening
  `'` between STRING literal and transpose OPERATOR via the
  `transpose` bool at `:389-394`.
- **OPERATOR** → `Operator`.
- **IDENTIFIER** — deliberately UNMAPPED. Framework
  convention (matches C / C++ / Pascal / VHDL / KIXtart /
  Caml / AutoIt / Ada / Verilog).
- **DOUBLEQUOTESTRING** → `String`. MATLAB R2017a+ `string`
  scalar-type literals. The language distinguishes them from
  char-arrays by TYPE but the user's eye reads both as
  string content, so they share the String slot.

Only KEYWORD is bold — matches the framework's "one bold
visual for language keywords" rule.

**Single-class install.** `matlabWordListDesc[]` at
`LexMatlab.cxx:516-519` declares one class only ("Keywords"),
plus the NULL sentinel. `MATLAB_THEME` installs class 0 alone.

Structural test coverage: 12 invariants — 7 style mappings
pin, single-class-only pin, `MATLAB_KEYWORDS` non-empty guard,
every-token-lowercase pin (dead-code prevention),
**contextual-keywords-must-be-absent** pin (`arguments` /
`properties` / `methods` / `events` MUST NOT appear —
protects the lexer's deliberate contextual promotion),
7 style-routing pins, `DEFAULT` + `IDENTIFIER` unmapped pins,
italic == 1 (`COMMENT`), bold == 1 (`KEYWORD` only), 3
cross-language non-reuse pins (Verilog / VHDL / Ada), and 20
MathWorks `iskeyword` anchor tokens.

**Haskell (2026-07-04):** uses Lexilla's `haskell` lexer
(`LexHaskell.cxx`, ~1120 lines) — a case-sensitive lexer for
Haskell 2010 plus common GHC extensions (MagicHash /
TemplateHaskell / TypeFamilies / SafeHaskell / literate
`.lhs` files). Three-class wordlist theme with 20 style
mappings covering the Haskell 2010 Language Report §2.4
reserved words, the Haskell 2010 FFI Addendum
callconv/safety qualifiers, and the §2.4 reserved-operator
punctuation set. Same file also registers
`SCLEX_LITERATEHASKELL` for `.lhs` files at
`LexHaskell.cxx:1119`; Code++ wires `SCLEX_HASKELL` only
today, but the literate-comment / codedelim SCE slots are
mapped defensively so a future `L_LHASKELL` langtype can
reuse the theme without a follow-up.

**Case-sensitive lexer.** `LexHaskell.cxx:747` matches
wordlist entries byte-exactly — no `tolower` fold. Haskell
identifier case carries syntactic meaning: a bare identifier
starting with an uppercase letter is a data constructor,
module name, or type name (dispatched to
`SCE_HA_CAPITAL` / `SCE_HA_MODULE` / `SCE_HA_DATA`); one
starting with lowercase is a value binding, function, or
type variable (`SCE_HA_IDENTIFIER`, unmapped per framework
convention). All Haskell 2010 §2.4 reserved words are
lowercase, so every `HASKELL_KEYWORDS` entry stays
lowercase.

**Context-driven state machine.** `LexHaskell` tracks a
`KeywordMode` state alongside the scan state; several
tokens are treated as contextual keywords via the mode
transitions and MUST NOT appear in `HASKELL_KEYWORDS`:

- `qualified` — recognized after `import` at
  `LexHaskell.cxx:756-759`, promoted to `SCE_HA_KEYWORD`
  and puts the lexer into `HA_MODE_IMPORT1` so subsequent
  capitalized names dispatch to `SCE_HA_MODULE`.
- `safe` — recognized after `import` when the
  `lexer.haskell.import.safe.highlight` option is on
  (`:760-764`).
- `as` and `hiding` — recognized after the `import M`
  name (`:766-771`, HA_MODE_IMPORT2 → HA_MODE_IMPORT3).
- `family` — recognized after `type` OR `data` (both enter
  `HA_MODE_TYPE` at `LexHaskell.cxx:793-795`) for the TypeFamilies
  GHC extension (`:772-774`).
- `forall` — RankNTypes quantifier; kept as an identifier
  by the plain lexer so pre-extension code doesn't
  over-highlight.

Test invariant #6 pins the absence of all five so a future
edit that "helpfully" adds one regresses the mode-driven
contextual behaviour visibly.

**Three wordlist classes:**

- **Class 0** (`HASKELL_KEYWORDS`, 22 tokens): Haskell 2010
  §2.4 reserved words — `case class data default deriving
  do else foreign if import in infix infixl infixr instance
  let module newtype of then type where`.
- **Class 1** (`HASKELL_FFI_KEYWORDS`, 13 tokens): FFI
  Addendum callconvs + safety qualifiers. Only recognized
  inside `foreign import` / `foreign export` (via
  `HA_MODE_FFI`), so entries like `ccall` or `safe` don't
  over-highlight ordinary identifiers with those names.
- **Class 2** (`HASKELL_RESERVED_OPERATORS`, 11 tokens):
  the §2.4 reserved-operator punctuation set —
  `.. : :: = \ | <- -> @ ~ =>`. Matched against
  operator-run tokens at `LexHaskell.cxx:645-654`.

**Style routing (20 mappings; `DEFAULT`, `IDENTIFIER`, and
the legacy `IMPORT` all unmapped):**

- **KEYWORD** → `Keyword` bold blue.
- **NUMBER** → `Number`. Covers decimal, scientific, hex
  (`0xFF`), octal (`0o755`), and MagicHash `#`-suffixed
  unboxed variants.
- **STRING + CHARACTER + STRINGEOL** → `String`. All three
  literal forms share the string lane; STRINGEOL matches
  the VHDL / Ada / Verilog / MATLAB precedent (visible in
  lane rather than deferred to a future Error slot).
- **CLASS + INSTANCE + CAPITAL + DATA** → `Keyword2`.
  Type-class heads, instance-head classes, bare
  capitalized identifiers (data constructors, type names,
  bare type applications), and data-declaration payload —
  all read as "known type-family identifier" in the
  language.
- **MODULE + PRAGMA + PREPROCESSOR +
  LITERATE_CODEDELIM** → `Preprocessor`. Module names in
  `import` / `module` context, `{-# LANGUAGE ... #-}`
  compiler pragmas, C-preprocessor `#`-directives, and
  the literate-Haskell `\begin{code}` / `>` delimiters
  are all out-of-band syntax markers.
- **OPERATOR + RESERVED_OPERATOR** → `Operator`. Ordinary
  user-defined operators and the §2.4 reserved set share
  the operator lane; a future palette could distinguish
  them since they emit through separate SCE indices.
- **COMMENTLINE + COMMENTBLOCK + COMMENTBLOCK2 +
  COMMENTBLOCK3 + LITERATE_COMMENT** → `Comment` italic.
  Nested block-comment depth is tracked by the lexer but
  paints identically at every depth.
- **IDENTIFIER** — deliberately UNMAPPED.
- **IMPORT (10)** — deliberately UNMAPPED. Legacy
  transitional state; modern LexHaskell (since 2013) routes
  import module names to `SCE_HA_MODULE` instead, so
  mapping this state would add a dead entry to the table.

Only KEYWORD is bold — matches the framework's "one bold
visual for language keywords" rule.

Structural test coverage: 12 invariants — 20 style
mappings pin, three-class order pin (0 / 1 / 2),
all-non-empty guard, every-token-lowercase pin for
KEYWORDS + FFI (reserved operators are punctuation, not
alphabetic), **contextual-keywords-must-be-absent** pin
(`qualified` / `safe` / `as` / `hiding` / `family`),
20 style-routing pins, DEFAULT + IDENTIFIER + IMPORT
unmapped pins (the IMPORT pin being the notable one —
protects against a well-meaning future edit that maps the
legacy state), italic == 5 (four comment-depth variants
plus literate-comment), bold == 1, 4 cross-language
non-reuse pins (MATLAB / Verilog / Caml / C++), and 22 +
3 + 5 anchor tokens across Keywords / FFI / reserved
operators.

**Inno Setup (2026-07-04):** uses Lexilla's `inno` lexer
(`LexInno.cxx`, ~380 lines) — a case-insensitive lexer for
the `.iss` installer-script format used by the Inno Setup
authoring tool. Written by Friedrich Vedder (2004). Compact
lexer with a six-class wordlist descriptor
(`Sections` / `Keywords` / `Parameters` /
`Preprocessor directives` / `Pascal keywords` /
`User defined keywords`), 11 style mappings, and 13
`SCE_INNO_*` slots.

**Case-insensitive lexer.** Inno Setup language semantics:
section names, directive names, and parameter names are all
case-insensitive. `LexInno.cxx:172` / `:191` / `:232` call
`tolower(ch)` on every byte before the wordlist InList
probe, so every wordlist entry MUST be lowercase — an
uppercase or mixed-case entry would be dead code (the probe
key is `appname` even though Inno source conventionally
spells directives in PascalCase). Test invariant #5 pins
every-token-lowercase across all five installed wordlists.

**Two-dimensional context dispatch.** The classifier decides
which wordlist to consult using both section context and
token-following punctuation:

1. `isCode` flag flips true after a `[Code]` section header
   (`LexInno.cxx:223`). While set, only `pascalKeywords`
   (class 4) is consulted for identifier tokens;
   `standardKeywords` / `parameterKeywords` / `userKeywords`
   are gated off. Consequence: putting a token in both the
   Pascal wordlist AND the standard-directive wordlist is
   safe — the section context decides which fires, not the
   lexer's within-token dispatch order.
2. Token-following punctuation. Class 1 (`SCE_INNO_KEYWORD`,
   Setup directives) fires only if the token is followed by
   `=` (`innoNextNotBlankIs(i, styler, '=')` at `:197`);
   class 2 (`SCE_INNO_PARAMETER`, section-item parameters)
   fires only if followed by `:` (`:199`). This is
   language-accurate — Inno distinguishes `AppName=...`
   (directive assignment) from `Name: ...` (section-item
   parameter assignment) — and means the same token can live
   in both wordlists without dead-entry concerns.

**Five wordlist classes installed; class 5 empty:**

- **Class 0** (`INNO_SECTIONS`, 18 tokens): canonical `.iss`
  section names — `[Setup]`, `[Files]`, `[Icons]`,
  `[Registry]`, `[Run]`, `[Tasks]`, `[Types]`, `[Components]`,
  `[Languages]`, `[Dirs]`, `[INI]`, `[UninstallRun]`,
  `[InstallDelete]`, `[UninstallDelete]`, `[Messages]`,
  `[CustomMessages]`, `[LangOptions]`, `[Code]`. The `[Code]`
  entry is special — matching it flips `isCode = true` at
  `LexInno.cxx:223`.
- **Class 1** (`INNO_KEYWORDS`, ~90 tokens): commonly-used
  `[Setup]`-section directive names covering app identity
  (`AppName` / `AppVersion` / `AppPublisher`), install
  locations (`DefaultDirName` / `DefaultGroupName`),
  wizard/UI settings, version constraints,
  compression/output, signing, privileges, and licence/info
  files.
- **Class 2** (`INNO_PARAMETERS`, ~50 tokens): section-item
  parameter names — `Source` / `DestDir` / `Flags` /
  `Filename` / `Parameters` / `Check` / `Root` / `Subkey` /
  etc.
- **Class 3** (`INNO_PREPROCESSOR`, ~20 tokens): ISPP
  (Inno Setup Preprocessor) directives — `define` /
  `include` / `if` / `else` / `endif` / `for` / `pragma` /
  etc.
- **Class 4** (`INNO_PASCAL_KEYWORDS`, ~50 tokens): Pascal
  Script reserved words for the `[Code]` section — the
  RemObjects Pascal Script dialect Inno uses, which is a
  Delphi/Object Pascal subset. Includes block structure
  (`begin` / `end`), declaration keywords (`var` / `const`
  / `type` / `function` / `procedure`), control flow, and
  the try/except/finally exception family.
- **Class 5** (User defined keywords) — NOT installed. Ships
  empty; the SCE state (`KEYWORD_USER`) is mapped
  defensively so a future per-project override populates it
  without a theme change.

**Style routing (11 mappings; `DEFAULT` and `IDENTIFIER`
unmapped):**

- **COMMENT + COMMENT_PASCAL** → `Comment` italic. Both the
  `;`-line comment used at script level and the Pascal
  `{...}` / `(*...*)` block comment used inside `[Code]`
  read as prose.
- **KEYWORD + KEYWORD_PASCAL** → `Keyword` bold blue. Setup
  directives and Pascal reserved words share the primary-
  keyword lane since they play the same role in their
  respective contexts (outer script vs `[Code]`).
- **PARAMETER + KEYWORD_USER** → `Keyword2` teal. Secondary
  structural accent for section-item parameter names and
  user-project identifiers.
- **SECTION + PREPROC** → `Preprocessor`. `[SectionName]`
  headers and `#`-directives are both structural markers
  rather than content.
- **INLINE_EXPANSION** → `Macro`. `{code:...}` / `{param:...}`
  inline preprocessor expansions get a distinct accent
  separate from PREPROC.
- **STRING_DOUBLE + STRING_SINGLE** → `String`. Both
  quote forms.
- **IDENTIFIER** — deliberately UNMAPPED per framework
  convention.

Structural test coverage: 12 invariants — 11 style mappings
pin, five-class order pin (0/1/2/3/4), all-non-empty guard,
every-token-lowercase pin, 11 style-routing pins, DEFAULT +
IDENTIFIER unmapped pins, italic == 2 (both comment forms),
bold == 2 (`KEYWORD` + `KEYWORD_PASCAL`), 3 cross-language
non-reuse pins (Haskell / MATLAB / Pascal), 6 canonical
section anchors, and 4+4+4+6 anchor tokens across
directives / parameters / preprocessor / Pascal.

**CMake (2026-07-04):** uses Lexilla's `cmake` lexer
(`LexCmake.cxx`, ~460 lines). 15 `SCE_CMAKE_*` slots
(0..=14), three-class wordlist (Commands / Parameters /
UserDefined). Distinctive feature: **mixed case-sensitivity
dispatch** driven by the CMake language's own semantics.

**Mixed case-sensitivity dispatch.** The classifier at
`LexCmake.cxx:105-165` builds BOTH `word` (preserved case)
and `lowercaseWord` (folded) buffers, then:

- Class 0 (`Commands`) probes `lowercaseWord` at `:135` —
  CMake commands are case-insensitive at the language level
  (`add_executable`, `ADD_EXECUTABLE`, `Add_Executable`
  all invoke the same command), so the wordlist entries must
  be lowercase and the lexer folds every candidate.
- Class 1 (`Parameters`) probes `word` byte-exact at `:138`
  — argument keywords are conventionally uppercase in CMake
  source (`PRIVATE`, `PUBLIC`, `REQUIRED`) and the lexer
  respects that convention.
- Class 2 (`UserDefined`) also byte-exact at `:142` —
  case-sensitive same as Parameters.

Host wordlist consequence: `CMAKE_COMMANDS` MUST be
all-lowercase (test invariant #5 pins this); `CMAKE_PARAMETERS`
is uppercase-by-convention matching CMake community style.

**Hard-coded flow-control keywords excluded from wordlists.**
The classifier at `:120-133` special-cases ten tokens with
`CompareCaseInsensitive` and dispatches them to their own
SCE states BEFORE the wordlist probe fires:

- `MACRO` / `ENDMACRO` → `SCE_CMAKE_MACRODEF`
- `IF` / `ENDIF` / `ELSEIF` / `ELSE` → `SCE_CMAKE_IFDEFINEDEF`
- `WHILE` / `ENDWHILE` → `SCE_CMAKE_WHILEDEF`
- `FOREACH` / `ENDFOREACH` → `SCE_CMAKE_FOREACHDEF`

Including any of these in `CMAKE_COMMANDS` would be dead
code — the classifier short-circuits at :120 — AND
documentation-misleading. Test invariant #6 pins their
absence.

**Syntactic non-wordlist dispatches.** Three SCE states fire
without any wordlist lookup:

- `SCE_CMAKE_VARIABLE` (7) at `:145-148` — any token whose
  second char is `{` and last char is `}` (i.e. `${var}`
  patterns).
- `SCE_CMAKE_NUMBER` (14) at `:150-162` — bare integer
  literal.
- `SCE_CMAKE_STRINGVAR` (13) at `:339-348` — variable
  interpolation `${var}` INSIDE any string state.

**Three wordlist classes:**

- **Class 0** (`CMAKE_COMMANDS`, ~95 tokens): CMake 3.x
  built-in commands — script control, target definition
  (`add_executable` / `add_library` / `add_subdirectory`),
  target-scoped configuration (`target_link_libraries`,
  `target_include_directories`, `target_compile_features`,
  `target_precompile_headers`), search (`find_package` /
  `find_library` / `find_path`), property introspection
  (`get_property` / `set_property` / `set_target_properties`),
  and deprecated-but-still-valid commands (`qt_wrap_cpp`,
  `variable_requires`).
- **Class 1** (`CMAKE_PARAMETERS`, ~190 tokens): argument
  keywords used across `target_link_libraries`
  (`PRIVATE`/`PUBLIC`/`INTERFACE`), `find_package`
  (`REQUIRED`/`QUIET`/`EXACT`/`COMPONENTS`), library-type
  qualifiers (`STATIC`/`SHARED`/`MODULE`/`OBJECT`), `set`
  cache qualifiers (`CACHE`/`FORCE`/`PARENT_SCOPE`),
  `file` / `list` / `string` operators
  (`GLOB`/`GLOB_RECURSE`/`APPEND`/`REGEX`/`MATCH`), `install`
  destination categories (`ARCHIVE`/`LIBRARY`/`RUNTIME`), `if`
  predicates (`DEFINED`/`EQUAL`/`STREQUAL`/`MATCHES`), and
  `message` severity levels
  (`STATUS`/`WARNING`/`FATAL_ERROR`).
- **Class 2** (`CMAKE_USER_DEFINED`) — ships empty. SCE
  state mapped defensively so a future per-project override
  populates it without a theme change.

**Style routing (14 mappings; `DEFAULT` unmapped):**

- **COMMENT** → `Comment` italic. CMake's `#` line comment
  (only comment form).
- **STRINGDQ + STRINGLQ + STRINGRQ** → `String`. Three
  string forms — `"..."` double-quoted (the only modern
  form), `` `...` `` backtick, and `'...'` single-quoted
  (both LQ and RQ are historical relics but the lexer
  still emits them).
- **COMMANDS + WHILEDEF + FOREACHDEF + IFDEFINEDEF +
  MACRODEF** → `Keyword` bold. All five fill the primary-
  keyword role; the four hard-coded flow-control SCE
  states exist so a future palette could differentiate
  flow keywords from ordinary commands, but for now they
  share the bold-blue accent.
- **PARAMETERS + USERDEFINED** → `Keyword2`. Secondary
  structural accent for argument keywords and user-project
  identifiers.
- **VARIABLE + STRINGVAR** → `Preprocessor`. `${var}`
  references (bare and inside strings) read as out-of-band
  syntax markers.
- **NUMBER** → `Number`.

Structural test coverage: 12 invariants — 14 style mappings
pin, three-class order pin, non-empty guard for classes 0/1
(class 2 permitted empty by design), every-token-lowercase
pin on `CMAKE_COMMANDS` (dead-code prevention),
**hard-coded-flow-control-must-be-absent** pin (10 tokens
protected), 14 style-routing pins, `DEFAULT` unmapped pin,
italic == 1 (`COMMENT`), bold == 5 (`COMMANDS` +
`WHILEDEF` + `FOREACHDEF` + `IFDEFINEDEF` + `MACRODEF`), 3
cross-language non-reuse pins (Inno / Haskell / MATLAB),
and 14 + 5 canonical CMake command + parameter anchors.

**YAML (2026-07-04):** uses Lexilla's `yaml` lexer
(`LexYAML.cxx`, ~370 lines). 10 `SCE_YAML_*` slots (0..=9),
single-class wordlist ("Keywords"). Distinctive feature:
**line-oriented scalar-value tokenizer** — the lexer treats
each source line as a `key: value` unit and dispatches the
value span through a dedicated classifier.

**Line-oriented state machine.** `ColouriseYAMLLine` at
`LexYAML.cxx:86-216` runs once per source line. It reads the
line into a fixed buffer, walks it once, and paints the whole
line in one of several structural forms:

- Leading `---` / `...` → `SCE_YAML_DOCUMENT` (document
  start/end marker, whole line).
- TAB in leading whitespace → `SCE_YAML_ERROR` (YAML forbids
  tab indentation outside block scalars).
- First non-space char is `#` → `SCE_YAML_COMMENT`.
- Continuation of a folded (`>`) or literal (`|`) block
  scalar → `SCE_YAML_TEXT` (indent-comparison against the
  parent line's stored state at `:99-109`).
- Otherwise: scan for the first unquoted `:` followed by
  whitespace or EOL → key at `SCE_YAML_IDENTIFIER`,
  separator at `SCE_YAML_OPERATOR`, value dispatched below.

**Value-position classifier.** Everything after `key: ` runs
through a short cascade:

- `&anchor` / `*alias` → `SCE_YAML_REFERENCE`.
- Wordlist match (byte-exact `InList` at `:188`) →
  `SCE_YAML_KEYWORD`. This is the only wordlist-driven
  dispatch in the entire lexer.
- Digits / `-` / `.` / `,` / spaces only → `SCE_YAML_NUMBER`
  (bare numeric scalar).
- Anything else → `SCE_YAML_DEFAULT` (plain unquoted string
  scalar).

**Wordlist semantics.** One class, one purpose: the value-
position boolean/null tokens. `LexYAML.cxx:188` calls
`KeywordAtChar` which delegates to `WordList::InList` — case-
exact, no folding. `YAML_KEYWORDS` ships the full YAML 1.1
§10.3-§10.4 spelling family (`y`/`Y`/`yes`/`Yes`/`YES` /
`true`/`True`/`TRUE` / `false`/`False`/`FALSE` /
`on`/`On`/`ON` / `off`/`Off`/`OFF` / `~`/`null`/`Null`/`NULL`
+ n-variants). YAML 1.2 restricts these to lowercase only but
almost every YAML parser in the wild still accepts YAML 1.1's
mixed-case forms; leaving the mixed-case variants
unhighlighted would be a real user-visible regression.

**`~` compact-null included.** YAML 1.1 §10.4 lists `~` as a
canonical null spelling equal in status to `null`/`Null`/`NULL`.
`WordList::InList` at `WordList.cxx:154-190` has exactly one
prefix special-case — `^` for a starts-with wildcard — and no
sigil-stripping for `~` or `%`; a one-byte entry `"~"` indexes
cleanly into `starts[0x7E]` and byte-compares to a match. `~`
is common in Ansible playbooks, Kubernetes manifests, Docker
Compose files, and Rails fixtures; test invariant #6 pins its
presence so a future edit doesn't silently drop it.

**Style routing (8 mappings; `DEFAULT` and `ERROR` unmapped):**

- **COMMENT** → `Comment` italic. `#`-to-EOL line comment
  (YAML's only comment form).
- **IDENTIFIER** → `Keyword2`. **Framework exception** — most
  `SCE_*_IDENTIFIER` states leave the slot unmapped so bare
  identifiers paint at `STYLE_DEFAULT`. YAML's IDENTIFIER is
  structurally the **key** of a mapping (the token before the
  first `:`), not a bare identifier, so it earns Keyword2 the
  same way `SCE_P_CLASSNAME` / `SCE_P_DEFNAME` /
  `SCE_PL_SUB_PROTOTYPE` / `SCE_PL_FORMAT_IDENT` route
  structural-name identifier states to Keyword2.
- **KEYWORD** → `Keyword` bold. Boolean/null value tokens —
  bold matches the C / Python / Rust primary-keyword archetype.
- **NUMBER** → `Number`.
- **REFERENCE** → `Preprocessor`. `&anchor` / `*alias` —
  structural cross-reference, out-of-band syntax marker.
- **DOCUMENT** → `Preprocessor` bold. `---` / `...` document
  boundaries — same "out-of-band structural marker" lane as
  REFERENCE; bold sets them apart while sharing the accent
  colour.
- **TEXT** → `String`. Content of folded / literal block
  scalars — verbatim string data spanning multiple lines.
- **OPERATOR** → `Operator`. The mapping-separator `:`.

**DEFAULT and ERROR unmapped.** Framework convention
(DEFAULT); framework has no Error slot (ERROR). Both fall
through to `STYLE_DEFAULT` which is the least-offensive
default — bare STYLE_DEFAULT keeps the buffer legible even
when the lexer emits states the theme doesn't colour.

Structural test coverage: 12 invariants — 8 style mappings
pin, single-class canonical-position pin, non-empty guard,
every-token-is-a-documented-YAML-boolean/null-spelling pin
(dead-code prevention: guards against a future edit accidentally
shipping arbitrary non-boolean/null tokens that byte-exact
`InList` would never match in a sensible file), `~`-presence
pin, 8 style-routing pins, `DEFAULT` + `ERROR` unmapped pins,
italic == 1 (`COMMENT`), bold == 2 (`KEYWORD` + `DOCUMENT`), 3
cross-language non-reuse pins (CMake / Inno / Haskell), and
11 canonical anchor tokens covering both YAML 1.2 lowercase,
one YAML 1.1 uppercase variant per boolean/null triple, and
the `~` compact-null spelling.

**COBOL (2026-07-04):** uses Lexilla's `COBOL` lexer
(`LexCOBOL.cxx`, 390 lines). 13 `SCE_COBOL_*` slots (0..=11
plus non-contiguous 16), three-class wordlist (A/B/Extended
Keywords). Dispatches `SCLEX_COBOL` (= 92, per
`SciLexer.h:108`). Distinctive features: **column-aware
lexer** (fixed-format COBOL comments), **case-fold
classification** (same discipline as CMake/Ada),
**non-sequential SCE numbering** (`SCE_COBOL_WORD2 = 16`,
not 12), and **A→B→C sequential-probe dispatch** at
`LexCOBOL.cxx:112-120`.

**Uppercase lexer name.** The `LANG_TABLE` row's
`lexer:` field is `Some("COBOL")` — uppercase, unique in
`LANG_TABLE` where every other lexer is lowercase. Registration
line `LexerModule lmCOBOL(SCLEX_COBOL, ColouriseCOBOLDoc,
"COBOL", ...)` (`LexCOBOL.cxx:390`) uses uppercase and Lexilla's
`CreateLexer` matches byte-exactly on the name — a well-
intentioned "normalise to lowercase" edit would silently
disable highlighting for `.cob`/`.cbl`/`.cpy` files. The
existing comment on the LANG_TABLE row (line 336) defends
the deviation and test invariant #1 pins the theme dispatch
so a regression fails loudly.

**Case-fold classification.** `LexCOBOL.cxx:76` inside
`getRange` writes `s[i] = tolower(styler[start+i])` into the
classification buffer BEFORE `WordList::InList` probes at
:107-121. COBOL is case-insensitive at the language level
(`MOVE`, `move`, `Move` are the same verb) and the lexer
folds every candidate; wordlist entries therefore MUST be
all-lowercase. Uppercase entries silently never match. Test
invariant #5 pins the discipline across all three lists.

**Non-sequential SCE numbering.** `SCE_COBOL_WORD2 = 16`
(not 12). The gap 12..=15 was reserved in the historical
Scintilla enum layout and never filled. A literal `12` in
the theme table would target a state `LexCOBOL` never emits,
silently breaking B-Keywords (PICTURE / VALUE / USAGE /
figurative constants) highlighting. Test invariant #6 pins
the value; all theme references use the named constant.

**Column-based dispatches** (no host cooperation required —
`LexCOBOL` handles fixed-format columns internally, all
inside `ColouriseCOBOLDoc`'s main state-machine loop):

- `column == 6` (indicator area col 7) with `*` or `/` →
  `SCE_COBOL_COMMENTLINE` (`LexCOBOL.cxx:215-218`).
- Inline `*>` anywhere (COBOL 2002+ free-format) →
  `SCE_COBOL_COMMENTLINE` (`:219-222`).
- `column == 0` with single `*` or `/` →
  `SCE_COBOL_COMMENTLINE` (`:223-228`).
- `column == 0` with `**` or `/*` →
  `SCE_COBOL_COMMENTDOC` (`:229-234`).
- `column == 0` with `?` → `SCE_COBOL_PREPROCESSOR` (rare)
  (`:241-243`).

**A-area division/section recognition.** The lexer tracks
`bAarea` (whether the current token starts in cols 1-2)
and hard-codes recognition of `division` / `declaratives`
/ `section` / `end` to compute fold-header levels via
bitflags (`IN_DIVISION` / `IN_DECLARATIVES` / `IN_SECTION` /
`IN_PARAGRAPH`, `LexCOBOL.cxx:122-142` inside
`classifyWordCOBOL`). These four tokens DO belong in
`COBOL_KEYWORDS_A` (they colour as structural verbs) but
their fold-level effect is separate from wordlist
highlighting — the bitflag computation runs on the same
lowercased buffer that fed the InList probes, via
`strcmp`.

**Hyphenated tokens are single lexemes.** `isCOBOLwordchar`
(`LexCOBOL.cxx:47-51`) accepts `-` as an identifier
character. `working-storage`, `end-if`, `date-written`,
`input-output`, `high-values`, `packed-decimal`, `comp-3`
are single lexemes — written literally with the hyphen.
Test invariant #7 pins the `[a-z0-9-]+` alphabet on every
wordlist token to catch a stray whitespace-inside-a-compound
edit.

**Three wordlist classes** (A→B→C sequential-probe order —
duplicates across lists shadow to the earlier list; test
invariant #8 enforces uniqueness):

- **Class 0** (`COBOL_KEYWORDS_A`, ~130 tokens): divisions
  (IDENTIFICATION / ENVIRONMENT / DATA / PROCEDURE),
  section names (WORKING-STORAGE / LINKAGE / FILE /
  CONFIGURATION / INPUT-OUTPUT), the top ~40 verbs by
  realistic-source frequency (MOVE / PERFORM / IF / DISPLAY
  / COMPUTE / OPEN / READ / WRITE / EVALUATE / etc.), all
  the explicit-scope terminators (END-IF / END-PERFORM /
  ... — 18 total), control-flow phrase words
  (THRU / VARYING / UNTIL / GIVING / etc.), preprocessor
  verbs (COPY / REPLACE — pull in copybooks / perform
  source-substitution), the EVALUATE fallback selector
  (OTHER), clause introducers attached to verbs (AT / ON
  / INVALID / SIZE / ERROR / OVERFLOW / EXCEPTION),
  arithmetic modifier (ROUNDED), OPEN modes (INPUT / OUTPUT
  / I-O / EXTEND), WRITE ADVANCING vocabulary (ADVANCING /
  BEFORE / AFTER), STRING/UNSTRING/INSPECT clause words
  (DELIMITED / DELIMITER / TALLYING / REPLACING / CONVERTING
  / CHARACTERS / LEADING / TRAILING), English relational
  operators (GREATER / LESS / EQUAL), the bare `END` +
  `DECLARATIVES` structural terminators, and `FUNCTION` as
  the intrinsic-call introducer (the callable names live
  in class C).
- **Class 1** (`COBOL_KEYWORDS_B`, ~90 tokens): PICTURE /
  VALUE / USAGE clauses (PICTURE / PIC / VALUE / OCCURS /
  REDEFINES / RENAMES / USAGE / JUSTIFIED / SYNCHRONIZED /
  SIGN / DEPENDING / INDEXED / KEY / etc.), the full USAGE
  mode family (BINARY / COMPUTATIONAL-1..5 / COMP-1..5 /
  PACKED-DECIMAL / POINTER / INDEX / NATIVE / DISPLAY-1 /
  NATIONAL), figurative constants (ZERO / ZEROS / ZEROES /
  SPACE / SPACES / HIGH-VALUE / HIGH-VALUES / LOW-VALUE /
  LOW-VALUES / QUOTE / QUOTES / NULL / NULLS / ALL),
  class-condition predicates (NUMERIC / ALPHABETIC /
  ALPHABETIC-UPPER / ALPHABETIC-LOWER), common qualifiers
  (FILLER / GLOBAL / EXTERNAL / IS / ARE / OF / IN / TO /
  TRUE / FALSE), and file descriptor keywords (FD / SD /
  SELECT / ASSIGN / ORGANIZATION / ACCESS / MODE /
  SEQUENTIAL / RANDOM / DYNAMIC / STATUS / LABEL / RECORD /
  BLOCK).
- **Class 2** (`COBOL_KEYWORDS_C`, ~14 tokens): COBOL 2002
  intrinsic function names — string (LENGTH / UPPER-CASE /
  LOWER-CASE / REVERSE / TRIM), numeric (NUMVAL / NUMVAL-C /
  INTEGER-OF-DATE / DATE-OF-INTEGER), date/time
  (CURRENT-DATE / WHEN-COMPILED), aggregation (MIN / MAX /
  SUM / MEAN / MEDIAN). **`random` deliberately excluded** —
  it collides with the SELECT ACCESS MODE `RANDOM` in list B,
  and the A→B→C probe order at `LexCOBOL.cxx:112-120` makes
  list B win regardless. `FUNCTION RANDOM(seed)` therefore
  renders at Keyword2 (list B) rather than Macro (list C);
  the user-visible cost is small and the correctness gain
  (list-B ACCESS MODE RANDOM painted correctly) is real.
  `LexCOBOL` has NO lookback for the preceding token — the
  `FUNCTION` introducer does not gate matches into this list.

**Style routing (11 mappings; `DEFAULT` and `IDENTIFIER`
unmapped):**

- **COMMENT + COMMENTLINE + COMMENTDOC** → `Comment` italic.
  Three comment forms collapse to one visual — matches the
  Lua / Tcl / Perl comment-family collapse precedent.
  COMMENT (state 1) is legacy — the current state machine
  doesn't emit it — but map defensively so a future Lexilla
  revision that revives it doesn't render un-coloured.
- **NUMBER** → `Number`. Also catches level numbers (`01`,
  `05`, `77`, `88`) intrinsically — those DON'T need
  wordlist entries.
- **WORD** (A) → `Keyword` bold. Primary structural
  vocabulary — verbs / divisions / sections / control flow.
- **STRING + CHARACTER** → `String`. Double-quoted and
  single-quoted literals collapse to one slot per the
  C / Perl / Ruby character-vs-string precedent.
- **WORD3** (C) → `Macro`. Intrinsic functions read as
  "known name from the runtime library"; Macro slot delivers
  the right visual weight without inventing a new slot.
  Mirrors Rust's `println!` at `SCE_RUST_MACRO`.
- **PREPROCESSOR** → `Preprocessor` bold. `?` at column 0
  (rare) — bold matches the C `#include`/`#define`
  precedent at `SCE_C_PREPROCESSOR`.
- **OPERATOR** → `Operator`.
- **WORD2** (B) → `Keyword2`. Secondary structural
  vocabulary — clauses / USAGE / figuratives / file
  descriptors. Colours the DATA and FILE sections.

**DEFAULT and IDENTIFIER unmapped.** Framework convention —
bare data names (`customer-record`, `total-amount`,
`account-balance`) paint at STYLE_DEFAULT rather than
picking up an arbitrary colour.

Structural test coverage: 14 invariants — 11 style
mappings pin, three-class canonical-order pin,
all-non-empty guard, every-token-lowercase pin (case-fold
discipline), `SCE_COBOL_WORD2 == 16` non-sequential-slot
pin, `[a-z0-9-]+` alphabet pin (identifier-char shape),
**cross-list uniqueness** (A/B/C intersection empty —
guards against dead-code duplicates under LexCOBOL's
first-match-wins A→B→C probe), 11 style-routing pins,
`DEFAULT` + `IDENTIFIER` unmapped pins, italic == 3 (all
three comment states), bold == 2 (`WORD` + `PREPROCESSOR`),
3 cross-language non-reuse pins (CMake / YAML / Haskell),
and 32 + 11 + 3 canonical COBOL anchor tokens across the
three lists plus the `random`-absent-from-C pin.

**Gui4Cli (2026-07-04):** uses Lexilla's `gui4cli` lexer
(`LexGui4Cli.cxx`, 315 lines, d. Keletsekis 2003). 10 slots
(0..=9) with **prefix `SCE_GC_`, not `SCE_GUI4CLI_`** —
Lexilla's own enum spelling, preserved on the host side for
greppability against the vendor tree. Five-class wordlist
(Globals / Events / Attributes / Control / Commands).
Distinctive features: **statement-position matching**
(keywords fire only on the leading token of a statement),
**uppercase case-fold** (same discipline as COBOL, inverted
from Ada/CMake), and **decoupled probe order vs descriptor
order** (Events probes LAST at classification time despite
being class 1 in the descriptor).

**Uppercase case-fold classification.** `LexGui4Cli.cxx:89-93`
walks the captured token buffer and does `*p = toupper(*p)`
BEFORE `WordList::InList` probes at `:105-109`. Gui4Cli is
case-insensitive at the language level (a 90s-era GUI
scripting language — keywords typed in any case, including
mixed like `xButton` / `xOnLoad` in Lexilla's own sample);
the lexer folds every candidate. Wordlist entries therefore
MUST be UPPERCASE — the same rule as COBOL. Test invariant
#5 pins the discipline across all five lists.

**Probe order at `:105-109` is NOT descriptor order.** The
classifier at classification time probes:

    Globals → Attributes → Control → Commands → Events

with Events LAST, first-match-wins. This is decoupled from
`gui4cliWordListDesc[]`'s declaration order at `:306-309`
(which `SCI_SETKEYWORDS` respects for the host-side install:
Globals=0, Events=1, Attributes=2, Control=3, Commands=4).
Consequence: a token appearing in both Globals and Events
resolves as Global — the Events entry is dead code. Test
invariant #8 pins cross-list uniqueness across all 10
pairwise combinations, guarding against a duplicate that
would silently mis-route highlighting.

**Statement-position matching only.** `colorFirstWord`
(`:72-120`) is invoked from the main dispatch at document
start, after every newline (`:226-236`), and after every
`;` statement terminator (`:191-202`). Keyword highlighting
fires ONLY for the leading token of a statement — the same
word appearing mid-statement stays `SCE_GC_DEFAULT`. E.g.
`LET a = GUIOPEN` will paint `GUIOPEN` as DEFAULT (mid-
statement), not as `SCE_GC_COMMAND`. This is a lexer
behaviour, not a host concern; users familiar with Gui4Cli
expect it. Not tunable from the host.

**Word-char alphabet extends beyond `[A-Z0-9_]`.**
`isAWordChar` at `:50-52` accepts letters, digits, `.`, `_`,
AND `\` (backslash) — so `path\to\file` reads as a single
identifier. The `\` escape dispatch at `:215-224` marks the
backslash + next character as `SCE_GC_OPERATOR` even inside
strings, then restores the prior state. Standard Gui4Cli
keyword identifiers stay within `[A-Z0-9_]`; test invariant
#7 pins that alphabet for the wordlists themselves.

**Fold points at Globals and Events.** `FoldGui4Cli` at
`:271-273` sets fold-header points on any line whose lead
token classifies as `SCE_GC_GLOBAL` or `SCE_GC_EVENT`. This
motivates the theme's bold-on-GLOBAL+EVENT choice — the two
classes are structural siblings in the folding model, so
they share bold weight to reinforce the visual pairing.

**Five wordlist classes** (~43 tokens total: 14+12+1+7+9, seeded from
`vendor/lexilla/test/examples/gui4cli/SciTE.properties` and
`AllStyles.gui` — the paired keyword-and-sample authored by
`d. Keletsekis, 2/10/2003` per `LexGui4Cli.cxx:6`. Non-seed
tokens are extrapolations from the naming conventions
established by the vendor seed and are explicitly marked
as such in the docstrings; unverified against a primary
Gui4Cli reference):

- **Class 0** (`GUI4CLI_GLOBALS`, 14 tokens): top-level
  control declarators — `G4C` (Gui4Cli script marker),
  `WINDOW`, `XBUTTON` (vendor-seed) plus 11 X-prefixed
  control names (`XCHECKBOX` / `XCOMBOBOX` / `XDROPLIST` /
  `XEDIT` / `XLISTVIEW` / `XPULLDOWN` / `XRADIO` / `XSTATIC`
  / `XTEXT` / `XTREEVIEW` / `XMENU`) extrapolated from the
  `XBUTTON` naming pattern.
- **Class 1** (`GUI4CLI_EVENTS`, 12 tokens): `X`-prefixed
  handler declarators — `XONLOAD` / `XONCLOSE` / `XONLVDIR`
  (vendor-seed) plus 9 additional `XON<event>` names
  (`XONCLICK` / `XONCHANGE` / `XONSELECT` / `XONKEY` /
  `XONMOUSE` / `XONTIMER` / `XONLVSELECT` / `XONDROP` /
  `XONMENU`) extrapolated from the vendor-seed naming
  pattern.
- **Class 2** (`GUI4CLI_ATTRIBUTES`, 1 token): the attribute-
  clause declarator — `ATTR`. Deliberately minimal — the
  vendor's own `SciTE.properties` keeps this list to `ATTR`
  alone because Gui4Cli attribute syntax is
  `attr <property> <value>` and `LexGui4Cli.cxx:72-120`
  (`colorFirstWord`) only probes wordlists for the LEADING
  token of a statement. Property names (`TEXTCOL`, `FONT`,
  etc.) appear at position 2 and never reach the wordlist
  dispatch — adding them to this list would be dead code.
- **Class 3** (`GUI4CLI_CONTROL`, 7 tokens): flow-control
  keywords that appear at leading statement position —
  `IF` / `ELSE` / `ENDIF` / `GOSUB` (vendor-seed) plus
  `GOTO` / `RETURN` / `EXIT`. **Explicitly excluded per
  the review pass:** `THEN` (Gui4Cli's `if` is block-form
  with implicit then — vendor sample writes `if $var >
  9999 ... endif` with no `then`); `AND` / `OR` / `NOT`
  (Gui4Cli uses symbolic operators `&`/`|`/`!` per
  `LexGui4Cli.cxx:204-205`, not English word forms — and
  these would appear mid-expression anyway, where wordlist
  dispatch never fires).
- **Class 4** (`GUI4CLI_COMMANDS`, 9 tokens): built-in
  verb vocabulary at leading statement position —
  `GUIOPEN` / `GUIQUIT` / `INPUT` / `MSGBOX` /
  `SETWINTITLE` (vendor-seed) plus the `GUI*` family
  extrapolations `GUICLOSE` / `GUIFRONT` / `GUIHIDE` /
  `GUISHOW`. **Explicitly excluded per the review pass:**
  `INPUTBOX` (vendor uses `Input`, no `InputBox` command
  exists); `GETTEXT` / `SETTEXT` / `GETVALUE` / `SETVALUE`
  / `ADDITEM` / `DELITEM` (Gui4Cli reads/writes widget
  state via dot-notation property access on the element
  handle — `$button.text`, `$edit.value` — not via these
  getter/setter commands); `PRINT` / `LET` / `SET` /
  `CALL` / `RUN` / `EXEC` / `WAIT` / `BEEP` (unverified
  against a primary Gui4Cli reference; vendor sample uses
  bare assignment `var = 9999`, not `LET var = 9999`).

**Style routing (9 mappings; `DEFAULT` unmapped):**

- **COMMENTLINE + COMMENTBLOCK** → `Comment` italic. `//`
  line comments and `/* ... */` block comments collapse to
  one visual — matches the Lua / Perl / Rust comment-family
  precedent.
- **GLOBAL** → `Keyword` bold. Primary structural anchor.
- **EVENT** → `Keyword2` bold. Secondary structural anchor,
  paired with GLOBAL as fold-header siblings.
- **ATTRIBUTE** → `Preprocessor`. Property/config markers
  read as out-of-band annotations — same lane as CMake's
  `${var}` and Rust's `#[attr]`.
- **CONTROL** → `Keyword`. Flow-control words share the
  primary-keyword accent with GLOBAL (not bold — semantic
  keyword weight without structural-anchor emphasis).
- **COMMAND** → `Macro`. Built-in verbs read as "callable
  from the runtime library" the same way Rust's `println!`
  reads. Macro slot delivers the visual weight without a
  new slot.
- **STRING** → `String`.
- **OPERATOR** → `Operator`. Arithmetic + relational +
  statement-terminator `;` + `$` variable sigil + `\`
  escape.

**DEFAULT unmapped.** Framework convention. Numeric
literals, `$var` identifier payloads (after the `$` sigil
paints as OPERATOR), and bare identifiers all fall through
to `STYLE_DEFAULT`. No dedicated Identifier or Number
state exists in `LexGui4Cli` — do not attempt to route one.

Structural test coverage: 13 invariants — 9 style mappings
pin, five-class canonical descriptor-order pin
(load-bearing for `SCI_SETKEYWORDS`), all-non-empty guard,
every-token-uppercase pin (case-fold discipline) with
vendor-seed anchors from `SciTE.properties` verified for
each of the five lists, `SCE_GC_OPERATOR == 9` numeric-
contract pin, `[A-Z0-9_]+` alphabet pin, **cross-list
uniqueness** across all 10 pairwise combinations of the
five lists (guards against dead-code duplicates under the
Globals → Attributes → Control → Commands → Events probe
order), 9 style-routing pins, `DEFAULT` unmapped pin,
italic == 2 (both comment states), bold == 2 (`GLOBAL` +
`EVENT` — the two fold-header structural anchors), and 3
cross-language non-reuse pins (COBOL / CMake / YAML).

**D (2026-07-04):** uses Lexilla's `d` lexer (`LexD.cxx`,
571 lines, Waldemar Augustyn 2006, folding by Udo Lechner
per `LexD.cxx:1-8`). 23 `SCE_D_*` slots (0..=22),
seven-class wordlist (WL0 primary keywords, WL1 storage
classes, WL2 Ddoc tags, WL3 types, WL4 specials, WL5 meta,
WL6 reserved user-extension). Dispatches `SCLEX_D` (= 79,
per `SciLexer.h:95`). Distinctive features:
**case-sensitive byte-exact matching** (D 2 is
case-sensitive at the spec level; wordlists lowercase-only,
inverted from COBOL / CMake), **`SCE_D_WORD3` declared but
never emitted** (LexD.cxx skips wordlist index 2 in
identifier classification — see below), **nested `/+ +/`
comments with per-line depth state**, **five string flavors
collapsing to one visual**, and **`.di` interface files
share the same lexer**.

**Case-sensitive classification.** `LexerD::LexerFactoryD`
at `LexD.cxx:198-200` constructs with `caseSensitive = true`.
The identifier-classification cascade at `:288-311` calls
`sc.GetCurrent(s, sizeof(s))` byte-exact when
`caseSensitive` is true, or `sc.GetCurrentLowered(...)`
when false. D 2 keywords are lowercase per spec, so
wordlist tokens are lowercase; `__UPPERCASE__` special
tokens like `__FILE__` also match byte-exact. Test invariant
#5 pins the discipline across all six populated wordlists.

**`SCE_D_WORD3` (value 8) unmapped by design.** SciLexer.h
declares 23 SCE_D_* slots but LexD's identifier
classification cascade at `LexD.cxx:296-307` probes
wordlists in the order 0/1/3/4/5/6 — **skipping index 2**.
Wordlist class 2 is instead used at `LexD.cxx:358` inside
the `SCE_D_COMMENTDOCKEYWORD` state (only entered from
`/** */` or `///` doc comments on a `@` / `\` sigil), NOT
in the identifier state. Consequence: `SCE_D_WORD3` is a
declared-but-never-emitted style; mapping it in the host
theme would be dead code. Test invariant #6 enforces the
exclusion.

**Cross-list uniqueness EXCLUDES wordlist class 2**
(Ddoc tags). Semantic overlap between `D_DOC_KEYWORDS`
(`return`, `deprecated`, `version`, `throw`) and
`D_KEYWORDS` (same tokens as D reserved words) is expected
and correct — the state machine dispatches on context
(WL2 only fires inside doc comments, WL0 only fires
outside them). Test invariant #8 checks pairwise
intersection only across WL0/WL1/WL3/WL4/WL5 which share
the identifier-classification cascade.

**Statement-position lexer quirks:**

- **Nested `/+ +/` comments carry per-line depth.**
  `SCE_D_COMMENTNESTED` state entered at `:443`;
  `curNcLevel` counter incremented/decremented at
  `:364-380` and `:439-444`, persisted per line via
  `styler.SetLineState` at `:263, :369, :377, :442`.
  Pure lexer concern — no host configuration.
- **Five string flavors, one visual slot.**
  `SCE_D_STRING` (`"..."`), `SCE_D_STRINGB` (`` `...` ``
  backtick wysiwyg), `SCE_D_STRINGR` (`r"..."`/`x"..."`/`q"..."`
  raw/hex/delimited), `SCE_D_CHARACTER` (`'c'`),
  `SCE_D_STRINGEOL` (unterminated). All five route to
  `StyleSlot::String` for uniform user-visible identity.
  String suffixes `c`/`w`/`d` are consumed via
  `IsStringSuffix` at `:63-65` (called at :387, :411, :418).
- **Doc-comment keyword state dual-return.**
  `SCE_D_COMMENTDOCKEYWORD` is entered from either
  `SCE_D_COMMENTDOC` (`/**`) or `SCE_D_COMMENTLINEDOC`
  (`///`) on the `@`/`\` sigil; `styleBeforeDCKeyword`
  remembers which to return to after the tag identifier.
  Wordlist class 2 validates the tag; invalid tags route
  to `SCE_D_COMMENTDOCKEYWORDERROR`.

**Seven wordlist classes** (~140 tokens across six
populated lists + one empty by design):

- **Class 0** (`D_KEYWORDS`, ~65 tokens): control flow +
  declarations + module system + access modifiers —
  `abstract`/`class`/`interface`/`template`/`mixin`/`module`/
  `import`/`if`/`else`/`while`/`for`/`foreach`/
  `foreach_reverse`/`switch`/`case`/`return`/`this`/
  `super`/`unittest`/`invariant`/`assert`/`is`/`typeid`/
  `typeof`/`cast`/`align`/`asm`/`pragma`/etc.
  Includes reserved-legacy tokens `body` (replaced by
  `do` in D 2.076 but reserved), `macro` (reserved with
  no spec meaning), and `delete` (deprecated post-GC).
- **Class 1** (`D_KEYWORDS_2`, ~10 tokens): type
  qualifiers + purity contracts + parameter storage —
  `const`/`immutable`/`shared`/`__gshared`/`pure`/
  `nothrow`/`lazy`/`ref`/`scope`.
- **Class 2** (`D_DOC_KEYWORDS`, ~16 tokens): JavaDoc/
  Doxygen-style tags used in doc comments —
  `param`/`return`/`throws`/`see`/`author`/`version`/
  `deprecated`/`bug`/`note`/`warning`/`example`/`since`/
  `todo`/etc. Bare tag names (no `@` sigil); the lexer
  probes `keywords3.InList(s + 1)` at `LexD.cxx:358` to
  skip the `@` prefix.
- **Class 3** (`D_TYPES`, ~30 tokens): primitive types
  from D spec §Types + standard aliases from `object.d` —
  `bool`/`char`/`wchar`/`dchar`/`byte`/`ubyte`/`short`/
  `ushort`/`int`/`uint`/`long`/`ulong`/`cent`/`ucent`/
  `float`/`double`/`real`/imaginary/complex/`void`/`string`/
  `wstring`/`dstring`/`size_t`/`ptrdiff_t`.
  Includes deprecated imaginary (`ifloat`/`idouble`/`ireal`)
  and complex (`cfloat`/`cdouble`/`creal`) types — removed
  from D 2 but reserved words per spec §2.4.5.
- **Class 4** (`D_SPECIAL`, ~15 tokens): boolean/null
  literals + compile-time source-location + environment
  tokens — `true`/`false`/`null`/`__FILE__`/
  `__FILE_FULL_PATH__` (D 2.083+)/`__LINE__`/`__MODULE__`/
  `__FUNCTION__`/`__PRETTY_FUNCTION__`/`__DATE__`/`__TIME__`/
  `__TIMESTAMP__`/`__VENDOR__`/`__VERSION__`/`__EOF__`.
- **Class 5** (`D_META`, 4 tokens): traits +
  meta-programming — `__traits`/`__vector`/`__parameters`/
  `__ctfe`.
- **Class 6** (`D_WORD7`, empty): reserved user-extension
  slot. Precedent: Rust doesn't ship Phobos-equivalent
  library surface in wordlists. Users who want Phobos
  functions to render as Keyword2 can populate this list
  via a project-level override; the `SCE_D_WORD7` slot is
  mapped defensively so the override takes effect without
  a theme change.

**Style routing (20 mappings; `DEFAULT`, `WORD3`,
`IDENTIFIER` unmapped):**

- **COMMENT + COMMENTLINE + COMMENTDOC + COMMENTNESTED +
  COMMENTLINEDOC + COMMENTDOCKEYWORDERROR** → `Comment`
  italic. Six comment forms collapse to one visual —
  matches the Lua / Tcl / Perl / COBOL comment-family
  collapse precedent. `COMMENTDOCKEYWORDERROR` maps to
  Comment (not Preprocessor) because a malformed doc tag
  is visually part of the surrounding comment, not a
  distinct out-of-band marker.
- **NUMBER** → `Number`. Recognises hex, binary,
  underscore digit separators, `e±`/`p±` scientific
  exponents, `f`/`F`/`L`/`i` suffixes.
- **WORD** (class 0) → `Keyword` bold. Primary structural
  vocabulary.
- **STRING + STRINGEOL + CHARACTER + STRINGB + STRINGR**
  → `String`. Five flavors, one slot — see string-flavor
  note above.
- **WORD2 + TYPEDEF + WORD5 + WORD6 + WORD7** (classes
  1/3/4/5/6) → `Keyword2`. Secondary vocabulary — storage
  classes, primitive types, special tokens, meta traits,
  reserved extension slot. All share the Keyword2 accent
  since they read as "annotations on structural code"
  rather than the primary vocabulary.
- **OPERATOR** → `Operator`. Punctuation + `@` attribute
  sigil.
- **COMMENTDOCKEYWORD** → `Macro`. Ddoc `@param`/`@return`
  etc. Read as "known name from the doc-tag vocabulary" —
  same semantic slot as Rust `println!` macro invocations.

**DEFAULT, WORD3, IDENTIFIER unmapped.** DEFAULT and
IDENTIFIER by framework convention (bare identifiers /
whitespace → STYLE_DEFAULT). WORD3 by the never-emitted
rule (see §above). Attribute names like `safe` / `nogc` /
`property` fall through IDENTIFIER because the `@` sigil
tokenizes separately as OPERATOR and the bare identifier
isn't a D reserved word; adding it to a wordlist would
create false-positives on ordinary variables.

**`.di` interface files.** L_D LangEntry's `extensions`
was extended from `["d"]` to `["d", "di"]` — D interface
files are auto-generated module headers (parallel to
`.h`/`.hpp` for C/C++). Same lexer.

Structural test coverage: 15 invariants — 20 style
mappings pin, seven-class canonical descriptor-order pin
(load-bearing for `SCI_SETKEYWORDS`), non-empty guard for
WL0-WL5 (WL6 permitted empty by design),
every-token-`[a-zA-Z0-9_]` pin (identifier alphabet),
`SCE_D_WORD3`-must-not-be-mapped pin
(dead-code-prevention for the declared-but-never-emitted
state), cross-list uniqueness across WL0/WL1/WL3/WL4/WL5
(WL2 EXCLUDED — Ddoc tags live in a separate lexer state
per the design rationale above), 20 style-routing pins,
`DEFAULT` + `IDENTIFIER` unmapped pins, italic == 6 (all
six comment states), bold == 1 (`WORD` — primary keyword
class), 3 cross-language non-reuse pins (COBOL / CMake /
YAML), 7 + 5 + 5 canonical D anchor tokens (spec §2.4.5
keywords + type primitives + specials), and `.di`
extension presence pin.

**PowerShell (2026-07-05):** uses Lexilla's `powershell`
lexer (`LexPowerShell.cxx`, 294 lines, Tim Gerundt 2008 per
`LexPowerShell.cxx:1-6`). 17 `SCE_POWERSHELL_*` slots
(0..=16), six-class wordlist (WL0 language keywords, WL1
cmdlets, WL2 aliases, WL3 well-known functions, WL4
reserved user-extension, WL5 comment-based-help tags).
Dispatches `SCLEX_POWERSHELL` (= 88, per `SciLexer.h:104`).
Distinctive features: **case-insensitive byte-lowered
matching** (all wordlists lowercase — same discipline as
COBOL / CMake / Gui4Cli, inverted from D), **hyphens are
word characters** (so `Get-ChildItem` tokenises as one
identifier and the wordlist entry is `get-childitem`),
**four string flavors collapsing to one visual** (double,
single, `@"..."@` here-string, `@'...'@` here-string),
**`$var` VARIABLE state maps to Lifetime slot** (matching
Perl / Bash / Rust sigil-tagged identifier archetype), and
**class 5 doc-help tags fire only inside `<# ... #>`
stream comments** (leading `.` sigil stripped at the
`keywords6.InList(s + 1)` probe).

**Case-insensitive classification.** `LexPowerShell` has
no `caseSensitive` factory switch. The identifier
classification cascade at `LexPowerShell.cxx:154-172` calls
`sc.GetCurrentLowered(s, sizeof(s))` unconditionally
before every `WordList::InList` probe. PowerShell
identifiers are compared with `OrdinalIgnoreCase` per
PowerShell Language Specification §7.4, so wordlist tokens
MUST be all-lowercase. Uppercase entries would silently
never match. Test invariant #5 pins the discipline across
all five populated wordlists.

**Hyphens are word characters.**
`LexPowerShell.cxx:32-34`'s `IsAWordChar` returns
`ch >= 0x80 || isalnum(ch) || ch == '-' || ch == '_'` —
so `-` extends the current identifier when NOT at position
0 of a token. Combined with the state-machine dispatch at
`:192-194` (`isoperator` fires BEFORE `IsAWordChar`), the
outcome is:

- **Leading `-`** — `isoperator('-')` is true, so
  `SCE_POWERSHELL_OPERATOR` state entered. Once the next
  char is non-operator, state exits back to DEFAULT which
  immediately enters IDENTIFIER on the bare word. This is
  how PowerShell's `-and`, `-eq`, `-like`, etc. tokenise:
  `-` as OPERATOR + bare word as IDENTIFIER. Consequence:
  wordlist entries for operator-word suffixes (`and`,
  `or`, `not`, `xor`, `band`, `bor`, `bnot`, `bxor`) DO
  fire in the identifier state, giving these tokens
  Keyword styling. Ship as-is.
- **Mid-token `-`** — no OPERATOR state transition, `-`
  extends the current IDENTIFIER token. Consequence:
  `Get-ChildItem` enters IDENTIFIER on `G`, extends through
  `-` and every char after, produces one lowered token
  `get-childitem` at the InList probe. Wordlist entries
  for cmdlets thus contain hyphens.

**WL4 (User1) ships empty by design.** Same precedent as
`D_WORD7` / Rust reserved. Third-party module cmdlets, DSC
resource names, and site-specific vocabulary are not
highlighted at the keyword level — populate this list via
a project-level override; the `SCE_POWERSHELL_USER1` slot
is mapped defensively so the override takes effect without
a theme change.

**Cross-list uniqueness EXCLUDES wordlist class 5**
(DocComment tags). Ddoc-style tokens like `synopsis`,
`description`, `example` are semantically unrelated to
identifier vocabulary but the state machine dispatches on
context (WL5 fires only inside the
`SCE_POWERSHELL_COMMENTDOCKEYWORD` state at `:107`, entered
from `SCE_POWERSHELL_COMMENTSTREAM` on a `.` sigil at
`:96-98`; WL0-WL4 fire only in identifier state at
`:154-172`). Test invariant #6 checks pairwise intersection
only across WL0/WL1/WL2/WL3.

**Statement-position lexer quirks:**

- **Four string flavors, one visual slot.**
  `SCE_POWERSHELL_STRING` (`"..."` — expands `$name`
  interpolation with `` ` `` escape at `:112-118`),
  `SCE_POWERSHELL_CHARACTER` (`'...'` — literal, no
  expansion at `:119-123`), `SCE_POWERSHELL_HERE_STRING`
  (`@"..."@` multi-line double-quoted at `:124-129`),
  `SCE_POWERSHELL_HERE_CHARACTER` (`@'...'@` multi-line
  single-quoted at `:130-135`). All four route to
  `StyleSlot::String` for uniform user-visible identity.
- **`#region` / `#endregion` folding.**
  `FoldPowerShellDoc` at `:247-259` walks
  `SCE_POWERSHELL_COMMENT` looking for these markers to
  open/close fold levels. Pure lexer concern — no host
  configuration.
- **`<# ... #>` stream comments fold** via a separate
  branch at `:241-246`.
- **Doc-comment keyword sigil-stripping.**
  `SCE_POWERSHELL_COMMENTDOCKEYWORD` is entered from
  `SCE_POWERSHELL_COMMENTSTREAM` on `.` + word char
  (`:96-98`), the wordlist probe strips the `.` via
  `keywords6.InList(s + 1)` at `:107`. Invalid tags fall
  back to `SCE_POWERSHELL_COMMENTSTREAM` via `ChangeState`
  at `:108`. Wordlist entries are BARE tag names (no `.`).

**Six wordlist classes:**

- **Class 0** (`POWERSHELL_KEYWORDS`, ~50 tokens):
  script-block openers (`begin` / `process` / `end` /
  `dynamicparam` / `clean`) + control flow (`break` /
  `continue` / `do` / `else` / `elseif` / `exit` / `for` /
  `foreach` / `from` / `if` / `in` / `return` / `switch` /
  `throw` / `trap` / `try` / `catch` / `finally` /
  `until` / `while`) + declarations (`class` / `enum` /
  `function` / `filter` / `param` / `hidden` / `static` /
  `data` / `define` / `var`) + module system (`using`) +
  Workflow reserved words (`workflow` / `inlinescript` /
  `parallel` / `sequence`) + operator-word suffixes
  (`and` / `or` / `not` / `xor` / `band` / `bor` /
  `bnot` / `bxor`). Language keywords sourced from
  Microsoft Learn `about_Language_Keywords` (36 table
  entries + 4 workflow entries). Operator-words sourced
  from `about_Logical_Operators` and
  `about_Bitwise_Operators` — they are `-and`/`-or`/etc.
  in source but the leading `-` tokenises as OPERATOR, so
  the bare-word suffix fires through identifier
  classification (see banner). `namespace` and
  `interface` deliberately excluded — neither is a
  documented reserved word per the spec.
- **Class 1** (`POWERSHELL_CMDLETS`, ~80 tokens): core
  cmdlets from `Microsoft.PowerShell.Management`,
  `Microsoft.PowerShell.Utility`, and
  `Microsoft.PowerShell.Core` — file/path operations,
  process/service management, output writers, object
  pipeline, formatting, module system, data interchange
  (CSV / JSON / XML), and remoting sessions. All stored
  lowercased and hyphenated as they lex.
  Provider-specific / platform-specific / third-party
  cmdlets excluded — those are best discovered via
  `Get-Command -Module` at runtime.
- **Class 2** (`POWERSHELL_ALIASES`, ~55 tokens): default
  aliases from `Get-Alias` output on Windows PowerShell
  5.1 — Unix-style (`cd` / `ls` / `cat` / `cp` / `mv` /
  `rm` / `pwd` / `curl` / `wget`) + cmd.exe-style
  (`dir` / `type` / `cls` / `copy` / `del` / `erase` /
  `rd` / `rmdir` / `md`) + three-letter cmdlet-family
  shortcuts (`sc` / `si` / `sv` / `sl` / `gc` / `gi` /
  `gci` / `gv` / `gp` / `sp` / `gm` / `gcm`) + Invoke-*
  shortcuts (`iex` / `icm`). `foreach` alias excluded —
  would collide with class 0 keyword. `?` / `%` excluded
  — non-word punctuation the lexer routes as OPERATOR.
- **Class 3** (`POWERSHELL_FUNCTIONS`, 11 tokens):
  shipped built-in functions per `Get-Command
  -CommandType Function` — `help` / `mkdir` / `oss` /
  `prompt` / `pause` / `more` / `clear-host` /
  `get-verb` / `tabexpansion` / `tabexpansion2` /
  `psedit`. `mkdir` lives here (not aliases) because
  it's a real function that wraps `New-Item -ItemType
  Directory`; the alias `md` resolves TO it.
- **Class 4** (`POWERSHELL_USER1`, empty): reserved
  user-extension slot. Same precedent as `D_WORD7`.
- **Class 5** (`POWERSHELL_DOC_KEYWORDS`, 15 tokens):
  comment-based-help tags per Microsoft Learn
  `about_Comment_Based_Help` — `synopsis` /
  `description` / `parameter` / `example` / `inputs` /
  `outputs` / `notes` / `link` / `component` / `role` /
  `functionality` / `forwardhelptargetname` /
  `forwardhelpcategory` / `remotehelprunspace` /
  `externalhelp`. Complete enumeration from the primary
  source — the "Comment-based help keywords" section of
  the doc lists exactly these 15 tokens.

**Style routing (15 mappings; `DEFAULT` and `IDENTIFIER`
unmapped):**

- **COMMENT + COMMENTSTREAM** → `Comment` italic. Two
  comment states (`#`-to-EOL vs `<# ... #>`) collapse to
  the same visual archetype — matches the Lua / Tcl /
  Perl / D comment-family collapse precedent.
- **NUMBER** → `Number`. Recognises decimals, hex
  (`0x...`), exponents, sign after `e`, and curated
  suffixes (`g` / `k` / `l` / `m` / `n` / `p` / `s` /
  `t` / `u` / `y`).
- **VARIABLE** → `Lifetime`. `$name` sigilled
  identifiers share the purple accent with Perl
  `$scalar`/`@array`/`%hash`/`*symbol_table`, Bash
  `$var`, and Rust `'lifetime`. Consistent visual
  grammar across every language whose lexer emits a
  sigilled-identifier state.
- **KEYWORD** (class 0) → `Keyword` bold. Primary
  structural vocabulary.
- **STRING + CHARACTER + HERE_STRING + HERE_CHARACTER**
  → `String`. Four flavors, one slot — see string
  discussion above.
- **CMDLET + ALIAS + FUNCTION + USER1** (classes 1/2/3/4)
  → `Keyword2`. Four secondary vocabulary classes share
  the Keyword2 accent since they all read as "known name
  from the language's callable dictionary" rather than
  the primary keyword vocabulary. USER1 mapped
  defensively for the reserved-extension use case.
- **OPERATOR** → `Operator`. Punctuation (`+`, `-`, `|`,
  `>`, `-and`/`-eq` leading `-`, etc.).
- **COMMENTDOCKEYWORD** → `Macro`. `.SYNOPSIS`/
  `.DESCRIPTION` etc. inside `<# ... #>` stream
  comments. Read as "known name from the doc-tag
  vocabulary" — same semantic slot as D's `@param` and
  Rust `println!` macro invocations.

**DEFAULT and IDENTIFIER unmapped.** DEFAULT by framework
convention (whitespace → STYLE_DEFAULT). IDENTIFIER by
framework convention so plain identifiers fall through
without host colouring — the lexer emits IDENTIFIER only
for tokens that failed all five wordlist probes at
`:154-169`.

Structural test coverage: 14 invariants — 15 style
mappings pin, six-class canonical descriptor-order pin
(load-bearing for `SCI_SETKEYWORDS`), non-empty guard for
WL0/1/2/3/5 (WL4 permitted empty by design), every-token
`[a-z0-9_-]+` pin (case-insensitive lowercase + hyphen
identifier alphabet), cross-list uniqueness across
WL0/WL1/WL2/WL3 (WL5 EXCLUDED — DocComment tags live in
a separate lexer state), 15 style-routing pins,
`DEFAULT` + `IDENTIFIER` unmapped pins, italic == 2
(both comment states), bold == 1 (`KEYWORD` — primary
keyword class), 3 cross-language non-reuse pins (D /
COBOL / YAML), 9 + 4 + 4 + 4 canonical PowerShell anchor
tokens, `.ps1` / `.psm1` / `.psd1` extension presence
pin, and WL5-no-leading-dot sigil-strip pin.

**R (2026-07-05):** uses Lexilla's `r` lexer (`LexR.cxx`,
350 lines, Neil Hodgson 1998-2002 per `LexR.cxx:1-6`). 16
`SCE_R_*` slots (0..=15), three-class wordlist (WL0 R
reserved words + logical / null / NA / Inf / NaN constants;
WL1 base package functions; WL2 other default-loaded
package functions — `stats` / `utils` / `graphics` /
`grDevices` / `methods`). Dispatches `SCLEX_R` (= 86, per
`SciLexer.h:102`). Distinctive features: **case-sensitive
byte-exact matching** (same discipline as D, inverted from
PowerShell / COBOL — R is spec-level case-sensitive so
`TRUE` and `true` are distinct identifiers), **`.` is a
mid-word character but NOT a word start** (so `is.numeric`
tokenises as one identifier; wordlist entries include
literal dots), **five string flavors collapsing to one
visual** (`"..."`, `'...'`, `` `name` `` backticked
non-standard name, R 4.0+ `r"(...)"` raw, R 4.0+
`r'(...)'` raw), **user-definable infix operators**
(`%in%`, `%*%`, `%o%`), and **descriptor's unused slots**
(RWordLists declares five entries but only classes 0/1/2
are probed — 3/4 are literally labelled "Unused").

**Case-sensitive classification.** `LexR.cxx:149` calls
`sc.GetCurrent(s, sizeof(s))` (byte-exact), NOT
`GetCurrentLowered`. R is case-sensitive at the spec
level, so wordlist tokens are spelled exactly as they
appear in R source — mostly lowercase, but constants
`TRUE`/`FALSE`/`NULL`/`NA`/`Inf`/`NaN` + the four
`NA_*_` variants are UPPERCASE, `NROW`/`NCOL` are
UPPERCASE, and `UseMethod`/`NextMethod` are CamelCase.
Test invariant #5 pins the case-sensitive alphabet across
all three wordlists.

**`.`-delimited identifiers are one token.**
`LexR.cxx:30-32`'s `IsAWordChar` returns
`(ch < 0x80) && (isalnum(ch) || ch == '.' || ch == '_')` —
so `.` mid-word extends the identifier. But `IsAWordStart`
at `:34-36` uses `(isalnum(ch) || ch == '_')` — no leading
dot. Consequence: `is.na`, `data.frame`, `as.character`,
`dev.off`, `t.test`, `read.csv`, `na.omit`, `install.packages`
tokenise as ONE identifier including the internal dot(s),
and wordlist entries include the dots verbatim. This is
essential for base R where `.`-separated names are the
convention.

**Cross-list uniqueness across ALL three classes.**
Unlike D (where WL2 lives in a separate lexer state), R's
three probed classes all share the identifier
classification cascade at `LexR.cxx:150-156` — a token in
two classes leaves the later entry dead code. Test
invariant #6 checks pairwise intersection across all
three. Deliberate placements:

- `mean` / `prod` / `sum` / `summary` / `sample` /
  `set.seed` → base (WL1) — they live in the `base`
  namespace even though users think of them as stats.
- `median` / `sd` / `var` / `cor` / `cov` / `quantile` /
  `IQR` / `mad` → stats (WL2) — the classic descriptive
  statistics functions live in the `stats` package.
- `read.csv` / `write.csv` / `str` / `head` / `tail` /
  `sessionInfo` → utils (WL2).
- `plot` → graphics/other (WL2) despite being promoted
  to `base` in R 4.0.0. Every R user's mental model
  places `plot` in `graphics`, and every base method
  dispatches to `graphics::plot.default`. Deliberate
  deviation from strict package origin.

**Descriptor's `Unused` slots.** `RWordLists[]` at
`LexR.cxx:339-346` declares five entries but the last two
are literally labelled `"Unused"` in the source. The paint
loop at `:146-158` only probes wordlists 0/1/2. The host
theme installs only three classes — installing 4 or 5
would be dead code. Same defensive rule as the D wiring's
"declared but never emitted" discipline.

**Statement-position lexer quirks:**

- **Five string flavors, one visual slot.**
  `SCE_R_STRING` (`"..."`), `SCE_R_STRING2` (`'...'`),
  `SCE_R_BACKTICKS` (`` `name` `` — non-standard names,
  used for reserved-word-like identifiers or column
  names with spaces), `SCE_R_RAWSTRING` (R 4.0+
  `r"(...)"` / `r"[...]"` / `r"{...}"` raw string,
  double-quoted), `SCE_R_RAWSTRING2` (R 4.0+ raw string,
  single-quoted). All five route to `StyleSlot::String`
  for uniform visual identity.
- **Raw strings with dash decorations.** R 4.0.0 (April
  2020) added raw strings with three delimiter families:
  `r"(...)"`, `r"[...]"`, `r"{...}"`, plus optional dash
  decorations for nested quotes like `r"-(...)-"`. Dash
  count + matching-delimiter state persist per line via
  `styler.SetLineState` at `:271-274`; parsed by
  `CheckRawString` at `:84-103`.
- **Infix operator state.** `SCE_R_INFIX` (10) covers
  R's user-definable `%...%` operators (`%%` modulo,
  `%in%` membership, `%*%` matrix multiplication, `%o%`
  outer product, `%/%` integer division). Entered at
  `:260-261` on `%`, exits on closing `%` at `:222-223`.
  `SCE_R_INFIXEOL` (11) is the error state for
  unterminated `%` reaching EOL — routes to Operator so
  an unclosed `%` doesn't paint like a comment.
- **Number literal recognition.** `SCE_R_NUMBER` (5)
  accepts decimals, hex (`0x`), scientific exponents
  (`e±`/`p±`), and R-specific suffixes `L` (integer
  literal) and `i` (imaginary/complex literal) per
  `LexR.cxx:134-144`.
- **`ESCAPESEQUENCE` opt-in.** `SCE_R_ESCAPESEQUENCE`
  (15) is emitted only when the host sets
  `lexer.r.escape.sequence` = 1 (default 0). Code++
  does not enable this property today, so the lexer
  never emits this state — the host theme leaves it
  unmapped (test invariant #8 enforces the exclusion).

**Three wordlist classes:**

- **Class 0** (`R_RESERVED`, 19 tokens): the canonical
  CRAN `?Reserved` list — control flow (`if` / `else` /
  `repeat` / `while` / `for` / `in` / `next` / `break`)
  + function definition (`function`) + logical constants
  (`TRUE` / `FALSE`) + null / math / NA sentinels
  (`NULL` / `NA` / `Inf` / `NaN`) + typed NA variants
  (`NA_integer_` / `NA_real_` / `NA_complex_` /
  `NA_character_`). Verbatim from
  `stat.ethz.ch/R-manual/R-devel/library/base/html/Reserved.html`.
  `T` / `F` deliberately EXCLUDED (per `?Reserved`,
  ordinary base variables bound to `TRUE`/`FALSE` at
  startup — user-rebindable, not parser-reserved).
  `return` deliberately EXCLUDED (base primitive
  function per `?return`, not a reserved word — lives in
  WL1). `...` deliberately EXCLUDED (tokenises as
  `SCE_R_OPERATOR`, not through identifier
  classification).
- **Class 1** (`R_BASE_FUNCTIONS`, ~180 tokens): the
  base package's user-facing functions — type
  predicates (`is.na` / `is.null` / `is.numeric` /
  `is.character` / `is.function` / etc.), type
  coercions (`as.numeric` / `as.character` / `as.Date`
  / etc.), constructors (`c` / `list` / `matrix` /
  `data.frame` / `factor`), aggregation (`sum` / `mean`
  / `min` / `max` / `length`), sequences (`seq` /
  `seq_len` / `seq_along` / `rep` / `rev`), apply
  family (`apply` / `sapply` / `lapply` / `mapply` /
  `tapply` / `vapply`), ordering (`sort` / `order` /
  `rank` / `which` / `match` / `unique`), set
  operations (`union` / `intersect` / `setdiff`),
  function primitives (`return` / `invisible` / `stop`
  / `warning` / `message`), package management
  (`library` / `require` / `attach` / `detach`), I/O
  (`print` / `cat` / `paste` / `sprintf` / `readLines`
  / `saveRDS`), string operations (`substr` /
  `toupper` / `strsplit` / `gsub` / `grep` / `grepl`),
  math primitives (`abs` / `sqrt` / `exp` / `log` /
  `floor` / `ceiling`), environment access
  (`environment` / `globalenv` / `assign` / `get` /
  `exists`), introspection (`class` / `typeof` /
  `attributes` / `names`), error handling (`tryCatch`
  / `try` / `conditionMessage`), object system
  (`UseMethod` / `NextMethod` / `structure` /
  `unclass`), sampling (`sample` / `set.seed`), the
  base generic `summary`, logical aggregators (`all` /
  `any` / `identical` / `xor`), functional-programming
  primitives (`Reduce` / `Filter` / `Map` / `Recall`), factor
  accessors (`levels` / `nlevels`), object manipulation
  (`unlist` / `do.call`), the `Sys.*` system family
  (`Sys.time` / `Sys.Date` / `Sys.getenv`), and the
  file path family (`file.exists` / `file.path` /
  `basename` / `dirname`). Sourced from the base
  package index at
  `stat.ethz.ch/R-manual/R-devel/library/base/html/00Index.html`.
- **Class 2** (`R_OTHER_FUNCTIONS`, ~90 tokens):
  functions from `stats` / `utils` / `graphics` /
  `grDevices` / `methods` — the other default-loaded
  packages. Descriptive statistics (`median` / `sd` /
  `var` / `cor` / `cov` / `quantile` / `IQR` / `mad`),
  modelling (`lm` / `glm` / `aov` / `predict` /
  `resid` / `coef` / `AIC` / `BIC`), hypothesis
  tests (`t.test` / `chisq.test` / `wilcox.test` /
  `cor.test`), data manipulation (`aggregate` /
  `formula` / `na.omit` / `nls`), GLM families
  (`gaussian` / `binomial` / `poisson`), RNG
  (`rnorm` / `runif` / `rbinom` / `rpois` / `rexp` /
  ... 9 more distributions), density/quantile
  functions (`dnorm` / `qnorm` / `pnorm` / `dunif` /
  `qunif` / `punif`), utils I/O (`read.csv` /
  `write.csv` / `read.table` / `head` / `tail`),
  utils package management (`install.packages` /
  `installed.packages` / `available.packages` /
  `download.file`), graphics plotting (`plot` /
  `hist` / `boxplot` / `barplot` / `pie` / `points`
  / `lines` / `abline` / `text` / `legend` / `axis`
  / `par` / `layout`), grDevices (`dev.new` /
  `dev.off` / `pdf` / `png` / `jpeg` / `svg` /
  `colors` / `rgb` / `hsv`), and methods (S4 class
  system: `setClass` / `setGeneric` / `setMethod` /
  `new` / `slot` / `slotNames` / `isVirtualClass` /
  `validObject` / `setRefClass`).

**Style routing (13 mappings; `DEFAULT`, `IDENTIFIER`,
`ESCAPESEQUENCE` unmapped):**

- **COMMENT** → `Comment` italic. Single `#`-to-EOL
  comment state.
- **NUMBER** → `Number`. Decimals, hex, scientific, `L`
  / `i` suffixes.
- **KWORD** (class 0) → `Keyword` bold. R reserved
  words + logical / null / NA / Inf / NaN constants.
- **STRING + STRING2 + BACKTICKS + RAWSTRING +
  RAWSTRING2** → `String`. Five flavors, one slot —
  see string discussion above.
- **BASEKWORD + OTHERKWORD** (classes 1/2) → `Keyword2`.
  Base and other-default-package functions share the
  Keyword2 accent since they all read as "known name
  from the language's callable dictionary" rather than
  parser-reserved vocabulary. Distinct SCE states are
  preserved so a future palette can split base vs
  stats coloring without a wordlist reshuffle.
- **OPERATOR + INFIX + INFIXEOL** → `Operator`.
  Punctuation plus R's user-definable `%...%` infix
  operators. `INFIXEOL` is the unterminated-infix
  error state — routes to Operator so an unclosed `%`
  doesn't paint like a comment.

**DEFAULT, IDENTIFIER, ESCAPESEQUENCE unmapped.**
DEFAULT and IDENTIFIER by framework convention
(whitespace / bare identifiers → STYLE_DEFAULT).
ESCAPESEQUENCE because the host doesn't enable the
`lexer.r.escape.sequence` property today — the lexer
never emits this state, so mapping it would be dead
code (test invariant #8 enforces the exclusion).

Structural test coverage: 15 invariants — 13 style
mappings pin, three-class canonical descriptor-order
pin (load-bearing for `SCI_SETKEYWORDS`), non-empty
guard for all three classes,
every-token-`[A-Za-z0-9._]` pin (R identifier alphabet
including case-sensitive uppercase constants + dots
mid-word), cross-list uniqueness across ALL three
classes (unlike D which excludes WL2 for state
separation — R's three classes all share the
identifier cascade so uniqueness is required), 13
style-routing pins, `DEFAULT` + `IDENTIFIER` +
`ESCAPESEQUENCE` unmapped pins, italic == 1 (COMMENT
only), bold == 1 (`KWORD` — primary keyword class), 3
cross-language non-reuse pins (D / PowerShell / COBOL),
11 + 8 + 6 canonical R anchor tokens (CRAN reserved +
canonical base + canonical stats/utils/graphics),
`.r` extension presence pin, and TWO negative pins
distinguishing R from convention-based highlighters:
`T`/`F` must NOT be in WL0 (user-rebindable per CRAN
`?Reserved`) and `return` must NOT be in WL0 (base
primitive per `?return` — lives in WL1).

**JSP (2026-07-05):** rides the shared `hypertext`
lexer (`LexHTML.cxx`) — same factory as HTML / PHP /
ASP / XML. There is **no `SCLEX_JSP`** — a JSP buffer
switches on lexer name `"hypertext"` per `L_JSP`'s
`LangEntry`, and Lexilla's HTML state machine handles
every JSP escape form generically without knowing they
are JSP-specific. No new `SCE_*` constants or new
keyword constants were added by this row; the wiring
is purely `HTML_KEYWORDS` (class 0) + `JAVA_KEYWORDS`
(class 1) + `HYPERTEXT_STYLES`.

**JSP escape-syntax coverage.** All four are already
covered by pre-existing hypertext lexer states:

- **`<%-- ... --%>` comment** → `SCE_H_XCCOMMENT` (20),
  labelled "ASP.NET, JSP Comment" per
  `LexHTML.cxx:878`. Already routes to `Comment`
  italic in `HYPERTEXT_STYLES`.
- **`<%@ page ... %>` / `<%@ taglib ... %>` /
  `<%@ include ... %>` directives** enter `SCE_H_ASPAT`
  (16) at `LexHTML.cxx:1645` unconditionally and stay
  in that state until the closing `%>`. `SCE_H_ASPAT`
  is labelled "preprocessor" per `LexHTML.cxx:874`, so
  the entire directive line renders in the
  `Preprocessor` colour — a coarser highlight than a
  naive reading might expect (individual tokens `page`
  / `import` / attribute values are NOT sub-styled),
  but internally consistent with how the hypertext
  lexer treats ASP.NET page directives. No dedicated
  JSP-directive wordlist needed.
- **`<% ... %>` scriptlet + `<%= ... %>` expression +
  `<%! ... %>` declaration** all three enter the
  server-side script state, which for the hypertext
  lexer means the `SCE_HJA_*` range at
  `LexHTML.cxx:913-926`. Lexilla has no "server Java"
  range — JSP's Java scriptlets share the `SCE_HJA_*`
  codepoints with ASP.NET server-side JScript. Class 1
  drives the word-coloring for both.
- **`${ ... }` and `#{ ... }` JSP EL** (Expression
  Language) parse as ordinary text runs — the hypertext
  lexer has no dedicated EL state, so EL expressions
  render at `SCE_H_DEFAULT`. Best-effort baseline;
  matches Notepad++'s JSP row.

**Class 1 = `JAVA_KEYWORDS` (deliberate choice).**
Unlike ASP (which installs `JAVASCRIPT_KEYWORDS` in
class 1 because ASP's server-side blocks are JScript by
default), JSP's `<% %>` scriptlets contain Java code.
Since class 1 also drives client-side `<script>` block
coloring (via `SCE_HJ_WORD`), the choice is a trade-off:

- **JAVA_KEYWORDS wins** for the server-side case —
  `public` / `private` / `class` / `extends` /
  `implements` / `import` / etc. all highlight in
  scriptlets. JSP files overwhelmingly weight toward
  server-side content.
- **Client-side JS blocks** in JSP files still get
  most keywords coloured: Java and JS share `if` /
  `else` / `while` / `for` / `do` / `switch` / `case` /
  `default` / `break` / `continue` / `return` / `try` /
  `catch` / `finally` / `throw` / `new` / `this` /
  `instanceof` / `const` / `true` / `false` / `null`.
  The JS-only tokens (`function` / `var` / `let` /
  `typeof` / `undefined` / `debugger` / `yield`) lose
  their highlight in client-side blocks. Conversely,
  Java-only tokens (`abstract` / `extends` /
  `implements` / `package` / `throws` /
  `synchronized`) would spuriously highlight if a
  client-side JS block happened to use those bare
  identifiers.

**Not installed: classes 2 / 3 / 4 / 5.** No VBScript,
Python, PHP, or SGML embedding in JSP. SGML markup
inside JSP still renders correctly via the shared
`HYPERTEXT_STYLES` SGML range without a wordlist
install (same pattern as HTML / XML / ASP). Structural
test invariant #4 enforces the "classes 0 and 1 only"
shape.

Structural test coverage: 7 invariants — 2-class
install shape, class 0 = canonical `HTML_KEYWORDS`
(shared with every other hypertext-family row),
class 1 = canonical `JAVA_KEYWORDS` (with a negative
`assert_ne!` against `JAVASCRIPT_KEYWORDS` to catch a
future regression that copy-pastes ASP's shape), no
classes 2/3/4/5 install, verbatim reuse of
`HYPERTEXT_STYLES` / `HYPERTEXT_ITALIC` /
`HYPERTEXT_BOLD` (same as HTML / PHP / ASP / XML),
cross-language non-reuse pins against every other
hypertext-family theme (HTML / PHP / ASP shapes are
each distinct), and `L_JSP` `LangEntry`'s `lexer:
Some("hypertext")` + `.jsp` extension presence.

**CoffeeScript (2026-07-05):** wires
`SCLEX_COFFEESCRIPT` (= 102) via `LexCoffeeScript.cxx`
— a Ruby-modelled state machine for CoffeeScript
source (`.coffee`, `.litcoffee`) built on top of the
LexCPP enum numbering. The lexer defines 26 SCE_*
enum slots (0..=25) but only enters 15 of them.
Eleven slots are never touched: 10 LexCPP-inherited
enum slots (`COMMENT` / `COMMENTDOC` / `UUID` /
`PREPROCESSOR` / `VERBATIM` / `COMMENTLINEDOC` /
`COMMENTDOCKEYWORD` / `COMMENTDOCKEYWORDERROR` /
`STRINGRAW` / `TRIPLEVERBATIM`) defined in
`SciLexer.h:1652-1672` but never referenced by any
`sc.SetState` / `sc.ChangeState` call, PLUS
`STRINGEOL` (12) — an **orphan case label** at
`LexCoffeeScript.cxx:262-266` whose switch branch
handles what to do WHILE in the state but which no
code path ever enters (confirmed by grep across the
vendored tree). Unterminated strings simply fall off
the STRING state at line end via the standard
state-machine reset path — they don't get a
distinctive error style. Theme leaves all 11
unmapped; test invariant #9 pins the deliberate
non-inclusion so a future maintainer doesn't
accidentally add dead work.

**Wordlist source of truth: `coffeescript/src/lexer.coffee`.**
Three wordlist classes install:
- **Class 0 → `SCE_COFFEESCRIPT_WORD`** —
  `COFFEESCRIPT_KEYWORDS` carries structural /
  control-flow / declaration / async keywords:
  `if else unless switch when then / for while
  until loop do / break continue return throw /
  try catch finally / class extends super new this
  / await yield debugger` (26 tokens). Rendered
  bold-blue.
- **Class 1 → `SCE_COFFEESCRIPT_WORD2`** —
  `COFFEESCRIPT_KEYWORDS_2` carries expression noise:
  word-form operators + aliases (`and or not is
  isnt typeof instanceof in of by delete`),
  boolean-literal aliases + value literals (`yes no
  on off true false null undefined NaN Infinity`),
  module-syntax words (`import export from as
  default`), contextual modifier `own`, and the two
  STRICT_PROSCRIBED identifiers `arguments` /
  `eval` (29 tokens). Rendered accent-color (not
  bold).
- **Class 3 → `SCE_COFFEESCRIPT_GLOBALCLASS`** —
  `COFFEESCRIPT_GLOBAL_CLASSES` carries 41 MDN
  Standard built-in objects: 1 `Array` + 1
  `Boolean` + 1 `Date`, the classic Error hierarchy
  (7 — `Error`/`EvalError`/`RangeError`/
  `ReferenceError`/`SyntaxError`/`TypeError`/
  `URIError`), the typed-array family (11 —
  `ArrayBuffer`/`DataView` + 9 typed arrays
  `Float32Array`/`Float64Array`/`Int8Array`/
  `Int16Array`/`Int32Array`/`Uint8Array`/
  `Uint8ClampedArray`/`Uint16Array`/`Uint32Array`),
  BigInt family (3 — `BigInt`/`BigInt64Array`/
  `BigUint64Array`), collection primitives (4 —
  `Map`/`Set`/`WeakMap`/`WeakSet`), general
  constructors + namespaces (11 — `Function`/`JSON`/
  `Math`/`Number`/`Object`/`Promise`/`Proxy`/
  `Reflect`/`RegExp`/`String`/`Symbol`), and host
  globals (2 — `console`/`globalThis`). Grouped by
  semantic category with alphabetical order within
  each group (wordlist ordering is a human-
  readability choice — Scintilla's
  `SCI_SETKEYWORDS` builds an internal hash for
  classification). Same slot as WORD2.

**Class-2 slot deliberately skipped.**
`csWordLists[]` at `LexCoffeeScript.cxx:486-492`
declares four slots but slot 2 is literally
labelled `"Unused"` — the identifier-classification
cascade at `:195-200` probes only slots 0, 1, and 3
(via `keywordlists[0]` / `[1]` / `[3]`). Installing
to slot 2 is dead `SCI_SETKEYWORDS` work. Test
invariant #3 pins the descriptor shape
`(0, KEYWORDS) (1, KEYWORDS_2) (3, GLOBAL_CLASSES)`
with an explicit `not-any(class == 2)` assertion.

**Style routing (13 mappings):**
- Two comment states + one verbose-regex-comment
  state → `Comment`. `COMMENTLINE` (2) is `#`-to-EOL;
  `COMMENTBLOCK` (22) is `###...###`;
  `VERBOSE_REGEX_COMMENT` (24) is `#`-to-EOL living
  inside a `///...///` verbose regex block.
- `WORD` (5) → `Keyword` (bold). `WORD2` (16) +
  `GLOBALCLASS` (19) + `INSTANCEPROPERTY` (25) →
  `Keyword2`. Three visual categories collapsed to
  one slot — same collapse discipline as R's
  BASEKWORD + OTHERKWORD and PowerShell's
  CMDLET + ALIAS + FUNCTION.
- `STRING` (6) + `CHARACTER` (7) + `REGEX` (14) +
  `VERBOSE_REGEX` (23) → `String`. Grouping regex
  under String matches the JavaScript / Ruby / Perl
  convention where regex delimiters visually read as
  string quotes.
- `NUMBER` (4) → `Number`. `OPERATOR` (10) →
  `Operator`.

**`@`-prefixed identifiers.** CoffeeScript's `@foo`
is shorthand for `this.foo` — the leading `@` starts
an identifier per `setWordStart` at
`LexCoffeeScript.cxx:124`, and the classifier at
`:200-202` detects the `@` prefix AFTER a wordlist
miss and re-styles the token as
`SCE_COFFEESCRIPT_INSTANCEPROPERTY` (25). Routed to
Keyword2 so `@name` gets the accent color that
signals "reference to an instance property".

**String interpolation via `#{...}`.** Ruby-style —
implementation borrowed from LexRuby at `:46-73`,
driven by stack-based tracking of up to 5
interpolation levels (`INNER_STRINGS_MAX_COUNT` at
`:139`). Enter at `:227` on `#{`, temporarily
paints `#{` as `OPERATOR`, then the expression
tokenises as normal CoffeeScript (keywords /
identifiers / numbers all get their normal styles
inside the interpolation) until the matching `}` at
`:329-335` restores the string state. Single-quoted
`CHARACTER` strings do NOT interpolate at `:238-246`
— matching CoffeeScript language semantics.

**Two regex flavours.** Inline `/pattern/flags`
regex (state `REGEX` (14), entered after operators
or keywords, exited on trailing `/` + lowercase
flags gobbling) and block `///pattern///` verbose
regex (state `VERBOSE_REGEX` (23), with `#`-to-EOL
comment support inside).

**Deliberate wordlist exclusions with test pins:**
- **`function` not in WL0** — CoffeeScript's
  `RESERVED` array at `lexer.coffee:1393-1398`
  actively rejects `function` in source; the parser
  errors on it (`->` and `=>` are the function-
  literal forms). Test invariant #15 pins the
  non-inclusion.
- **`NaN` / `Infinity` in canonical case (WL1)** —
  byte-exact classifier at
  `LexCoffeeScript.cxx:193-203` means lowercase
  `nan` / `infinity` would silently miss. Test
  invariant #16 pins the canonical case.
- **`Infinity` / `NaN` / `undefined` / `null` /
  `true` / `false` not duplicated in WL3** — they
  live in WL1 (secondary keywords). First-match-wins
  probe order at `:195-200` means a duplicate in
  WL3 is dead code. Test invariant #6 (cross-list
  uniqueness) pins the invariant.
- **DOM instances (`window` / `document` /
  `navigator`) not in WL3** — these are host-
  provided instance references, not class
  constructors; wrong bucket for `GLOBALCLASS`.
  Documented in `COFFEESCRIPT_GLOBAL_CLASSES` docstring.

Structural test coverage: 16 invariants —
`Some(&COFFEESCRIPT_THEME)` return, 13-mapping
style count, three-class canonical descriptor order
`(0, KEYWORDS) (1, KEYWORDS_2) (3, GLOBAL_CLASSES)`
with explicit `class == 2` exclusion, all classes
non-empty, identifier-alphabet enforcement,
cross-list uniqueness across WL0/WL1/WL3,
style-routing pins for the 13 mapped SCE
constants, DEFAULT (0) + IDENTIFIER (11)
unmapped, all 11 never-entered states unmapped
(10 LexCPP-inherited + STRINGEOL orphan), italic
set == 3 (all comment states), bold set == 1
(WORD only), cross-language non-reuse against R
/ PowerShell / D / COBOL, `L_COFFEESCRIPT`
`LangEntry`'s `lexer: Some("coffeescript")` +
`.coffee` extension presence, canonical anchor
coverage (WL0 primary keywords, WL1 mix of aliases
+ literals + module noise, WL3 canonical global
classes), and TWO negative pins: `function` NOT
in any wordlist (WL0/WL1/WL3 — parser rejects it)
and `NaN` / `Infinity` in WL1 use canonical case
(byte-exact classifier).

**JSON / JSON5 (2026-07-05):** wires `SCLEX_JSON`
(= 120, per `SciLexer.h:136`) end-to-end. **Both
`L_JSON` and `L_JSON5` share a single `JSON_THEME`
via a `L_JSON || L_JSON5` dispatcher branch** —
they route to the same Lexilla lexer per
`LANG_TABLE`, use identical wordlists, and differ
only in the extensions their `LangEntry` rows
match (`.json` vs `.json5`). Test invariant #2
pins the pointer-equality of the two theme lookups
to catch a future copy-paste that duplicates the
theme.

**Two Lexilla properties enabled by the host** via
`extra_fold_properties`'s `L_JSON` / `L_JSON5`
branch:
- `lexer.json.escape.sequence = 1` lights up
  `SCE_JSON_ESCAPESEQUENCE` for `\\uHHHH` / `\\n`
  / etc. inside strings. Notepad++ default.
- `lexer.json.allow.comments = 1` lights up
  `SCE_JSON_LINECOMMENT` (`//`) and
  `SCE_JSON_BLOCKCOMMENT` (`/* */`). Strict RFC
  8259 JSON forbids these; JSON5 and JSONC (JSON
  with Comments) permit them. Enabling for both
  languages matches Notepad++ / VSCode defaults —
  an errant `//` in a `.json` file renders as a
  comment rather than an ERROR blob (still
  parse-invalid, but the text editor is not a
  validator).

The name `extra_fold_properties` is a historical
misnomer — the helper emits any per-lang Lexilla
properties, not just `fold.*` ones. A future
rename to `extra_lexer_properties` is tracked as
follow-up.

**Wordlists (source of truth: RFC 8259 §3 for JSON
literals; JSON5 spec §4.2 for `Infinity` / `NaN`;
W3C JSON-LD 1.1 §"Keywords" for the 23 `@`-keywords).**
- **Class 0 → `SCE_JSON_KEYWORD`** —
  `JSON_KEYWORDS` carries the three RFC 8259
  literals (`true`, `false`, `null`) plus the two
  JSON5 numeric-literal extensions (`Infinity`,
  `NaN`). `undefined` explicitly excluded per
  invariant #17 — JSON5's spec does NOT include
  it (that's JavaScript-only).
- **Class 1 → `SCE_JSON_LDKEYWORD`** —
  `JSON_LD_KEYWORDS` carries all 23 documented
  JSON-LD 1.1 keywords (`@base`, `@container`,
  `@context`, `@direction`, `@graph`, `@id`,
  `@import`, `@included`, `@index`, `@json`,
  `@language`, `@list`, `@nest`, `@none`,
  `@prefix`, `@propagate`, `@protected`,
  `@reverse`, `@set`, `@type`, `@value`,
  `@version`, `@vocab`). **The `@` prefix is
  carried in the wordlist entry** — the byte-exact
  classifier at `LexJSON.cxx:191-206` reads the
  `@` as part of the word; an entry without it
  would silently never match. Test invariant #7
  pins this shape.

**Style routing (13 mappings):** `NUMBER` (1) →
`Number`; `STRING` (2) + `STRINGEOL` (3) →
`String`; `PROPERTYNAME` (4) + `URI` (9) +
`COMPACTIRI` (10) + `LDKEYWORD` (12) → `Keyword2`
(four visual identifier categories collapse to
one accent-color slot — same collapse discipline
as R's `BASEKWORD` + `OTHERKWORD`, PowerShell's
`CMDLET` + `ALIAS` + `FUNCTION`, and
CoffeeScript's `WORD2` + `GLOBALCLASS` +
`INSTANCEPROPERTY`); `ESCAPESEQUENCE` (5) →
`Preprocessor` (out-of-band syntax marker inside
strings); `LINECOMMENT` (6) + `BLOCKCOMMENT` (7)
→ `Comment` italic; `OPERATOR` (8) → `Operator`;
`KEYWORD` (11) → `Keyword` bold; `ERROR` (13) →
`Preprocessor` (attention marker — makes parse
errors visible rather than invisibly rendered at
`STYLE_DEFAULT`; Code++'s `StyleSlot` enum has no
dedicated "error red" so `Preprocessor` is the
closest available signal). `DEFAULT` (0) unmapped
per framework convention. `SCE_JSON_URI` sub-style
fires for seven URI-scheme prefixes inside a
string (`https://`, `http://`, `ssh://`, `git://`,
`svn://`, `ftp://`, `mailto:`) per
`LexJSON.cxx:347-353` — URLs inside JSON string
values get accent-color highlighting the same way
IDE URL detectors do it. `SCE_JSON_COMPACTIRI`
fires on the end-quote of a string containing
exactly one `:` with every other char in
`CompactIRI::setCompactIRI` (alpha + `$_-`
per `LexJSON.cxx:59`) — the JSON-LD compact IRI
form (`prefix:suffix`).

**Property-name detection is lookahead-driven.** A
`"..."` string entered from `DEFAULT` is
re-classified to `SCE_JSON_PROPERTYNAME` at
`LexJSON.cxx:407-410` if `AtPropertyName` at
`:171-189` finds the closing quote followed by
(up-to-50-spaces of whitespace and then) `:`.
This distinguishes JSON object keys from string
values visually without a grammar change.

**`SCE_JSON_ERROR` catches everything the lexer
can't classify.** At `LexJSON.cxx:455-457` any
non-whitespace char that doesn't match a
state-entry condition transitions to ERROR. This
includes bare identifiers not in the keyword
wordlist (e.g., `undefined` in a JSON5 file, which
we explicitly exclude from the wordlist),
unterminated escapes, and stray punctuation.
Routing ERROR to `Preprocessor` makes these
visible in the editor.

Structural test coverage: 18 invariants —
`Some(&JSON_THEME)` return for BOTH `L_JSON` and
`L_JSON5` (with pointer-equality pin catching a
copy-paste divergence), 13-mapping style count,
two-class canonical descriptor order matching
`JSONWordListDesc[]`, both classes non-empty, WL0
alphabet `[A-Za-z]+`, WL1 shape `@[a-z]+`
(`@`-prefix required), cross-list uniqueness,
style-routing pins for the 13 mapped SCE
constants, `DEFAULT` (0) unmapped, `ERROR` →
`Preprocessor` specific pin, italic set == 2
(both comment states), bold set == 1 (`KEYWORD`
only), cross-language non-reuse against R /
CoffeeScript / PowerShell, `L_JSON` `LangEntry`'s
`lexer: Some("json")` + `.json` extension AND
`L_JSON5` `LangEntry`'s `lexer: Some("json")` +
`.json5` extension, canonical anchor coverage
(WL0 all five literals, WL1 five representative
JSON-LD keywords), ONE negative pin (`undefined`
NOT in WL0 — JSON5 spec doesn't include it), and
Lexilla property enablement (both
`lexer.json.escape.sequence = "1"` AND
`lexer.json.allow.comments = "1"` in
`extra_fold_properties` for BOTH `L_JSON` and
`L_JSON5`).

**JavaScript (2026-07-05):** rides `LexCPP` (per
`L_JAVASCRIPT`'s `LangEntry.lexer: Some("cpp")`) —
same shared `CPP_STYLES` / `CPP_ITALIC` /
`CPP_BOLD` reused across the LexCPP family (C /
C++ / C# / Java / Objective-C / RC). No new
`SCE_*` constants and no new theme table needed;
the wiring is purely a class-0 + class-1 keyword
pair.

**Class 0 (`SCE_C_WORD`, bold blue) —
`JAVASCRIPT_KEYWORDS`** already existed
pre-Phase-4.5 (installed as class 1 of the
hypertext lexer's `htmlWordListDesc[]` for
embedded `<script>` blocks). This commit binds
the same wordlist to LexCPP class 0 for `.js`
files. 49 tokens covering ES5 reserved words
(`if` / `for` / `function` / `var` / …) + ES2015+
block-scoped bindings (`let` / `static`) + ES2017+
coroutines and contextual `of` (`async` / `await`
/ `of`) + strict-mode future-reserved
(`implements` / `interface` / `package` /
`private` / `protected` / `public`) + language
literals (`true` / `false` / `null` /
`undefined`). Sourced and adversarially verified
against ECMAScript 2024 spec / Notepad++ baseline
/ hypertext-lexer source.

**Class 1 (`SCE_C_WORD2`, accent steel-blue) —
new `JAVASCRIPT_KEYWORDS_2`** carries 51 MDN
Standard built-in objects covering the natural
class-1 role in the LexCPP-family convention
("type-like tokens"). Since JavaScript has no
C-style primitives, the class-1 population is the
built-in **constructors** and **namespace / global
value** identifiers:

- General wrappers (12): `Array`, `Boolean`,
  `Date`, `Function`, `JSON`, `Math`, `Number`,
  `Object`, `RegExp`, `String`, `Symbol`,
  `BigInt`.
- Concurrent + iteration primitives (4):
  `Promise`, `Proxy`, `Reflect`, `Iterator`.
  `Iterator` is ES2025 Iterator Helpers
  (`Iterator.from(...)`, `Iterator.prototype.map`
  / `.filter` / `.take` / `.drop`) — Stage 4
  reached 2024, shipping in Chrome 122+,
  Firefox 131+, Node 22+.
- Collection primitives (5): `Map`, `Set`,
  `WeakMap`, `WeakSet`, `WeakRef`.
- Error hierarchy (8): `Error`, `EvalError`,
  `RangeError`, `ReferenceError`, `SyntaxError`,
  `TypeError`, `URIError`, `AggregateError`.
- Buffer / view primitives (3): `ArrayBuffer`,
  `DataView`, `SharedArrayBuffer`.
- Typed-array family (12): `Float16Array`,
  `Float32Array`, `Float64Array`, `Int8Array`,
  `Int16Array`, `Int32Array`, `Uint8Array`,
  `Uint8ClampedArray`, `Uint16Array`,
  `Uint32Array`, `BigInt64Array`, `BigUint64Array`.
  `Float16Array` is ES2025 Stage 4 (December
  2024), shipping in Chrome 135+, Safari 18.4+,
  Firefox 137+.
- Namespace globals (3): `Intl`, `Atomics`,
  `WebAssembly`.
- Language / host globals (4): `globalThis`,
  `console`, `NaN`, `Infinity`. `NaN` and
  `Infinity` are ECMAScript §21.1 Value
  Properties of the Global Object — canonical
  built-in globals same category as
  `console` / `globalThis`. They are NOT in
  `JAVASCRIPT_KEYWORDS` class 0 (that wordlist's
  docstring lists them under "Deliberate
  exclusions → Global objects and host APIs" —
  the exclusion applies to class 0 where they'd
  render bold as "keywords"; class 1
  accent-color is the correct home).

Total: 12 + 4 + 5 + 8 + 3 + 12 + 3 + 4 = 51.

**Class-0 vs class-1 rationale.**
`JAVASCRIPT_KEYWORDS`'s docstring lists these
tokens under "Deliberate exclusions" — "identifiers
bound at runtime, not keywords. Highlighting them
would mis-colour a user's local
`const Math = ...` shadow." That reasoning is
correct for class 0 (bold "Keyword" slot — reserved
for **parser keywords**). It does NOT extend to
class 1 (accent "Keyword2" slot), which by
LexCPP-family convention holds *type-like tokens*.
For JS this maps naturally onto the built-in
constructors and namespaces — matching what VS
Code / IntelliJ / Sublime / Notepad++ all colour
distinctly. The "user shadows Math" edge case is
dwarfed by the discoverability win of
highlighting recognised built-ins.

**Deliberate exclusions from class 1** (with test
pins):

- **DOM instances** (`window`, `document`,
  `navigator`, `localStorage`) — browser runtime
  globals, not ECMAScript built-ins. Node.js
  `.js` files wouldn't have them. Test pin
  asserts these four names are NOT in the
  wordlist.
- **Value literals in class 0** (`true`, `false`,
  `null`, `undefined`) — already in class 0.
  LexCPP's classifier probes class 0 first, so a
  class-1 duplicate is dead code. Test pin
  asserts these four names are NOT in class 1
  AND that they ARE in class 0. Cross-list
  uniqueness `HashSet` intersection catches any
  future edit that moves a token to class 1
  without dropping it from class 0. NOTE:
  `NaN` and `Infinity` are NOT excluded — they
  correctly live in class 1 as ECMAScript §21.1
  global values.
- **`FinalizationRegistry`** — real ES2021 global
  but vanishingly rare in practice. Documented
  exclusion; a future contributor can add it if
  usage patterns change.
- **`GeneratorFunction` / `AsyncFunction` /
  `AsyncGeneratorFunction`** — NOT global
  identifiers. Only reachable via
  `(function*(){}).constructor` etc.
  Highlighting them would highlight tokens that
  never appear in valid code.
- **DOM method names** (`getElementById`,
  `querySelector`, `addEventListener`) — methods
  on host objects, not global identifiers.
- **Library-specific globals** (jQuery `$`, lodash
  `_`) — third-party, not language built-ins.

Structural test coverage — the dedicated
`javascript_reuses_lexcpp_style_table_and_canonical_keywords`
test pins style-table reuse (CPP_STYLES /
CPP_ITALIC / CPP_BOLD share with C / C++), class-0
= `JAVASCRIPT_KEYWORDS`, class-1 =
`JAVASCRIPT_KEYWORDS_2`, class-0 divergence from
Java's list, class-1 divergence from Java's
primitive list, `Array` present as archetypal
class-1 anchor, 11-anchor spot-check across every
sub-category, DOM-instance and value-literal
absence pins, and cross-list uniqueness via
`HashSet::intersection`. Meta-test
`lexcpp_family_installs_class_0_and_class_1`
extended with `L_JAVASCRIPT`;
`wired_languages_have_complete_themes` extended
with `L_JAVASCRIPT`.

**Fortran fixed + free form (2026-07-05):** wires
`SCLEX_FORTRAN` (= 36, free-form) and `SCLEX_F77`
(= 37, fixed-form) end-to-end. **Both `L_FORTRAN`
(id 25) and `L_FORTRAN_77` (id 59) share a single
`FORTRAN_THEME` via a `L_FORTRAN || L_FORTRAN_77`
dispatcher branch** — one `LexFortran.cxx` exports
two `LexerModule` instances (`:723-724`) that share
`ColouriseFortranDoc` with just an `isFixFormat`
boolean toggling column-oriented parsing at
`:92-122` (columns 1-5 label / column 6
continuation / column 72+ comment). Same SCE_F_*
enum, same three-class wordlist descriptor at
`:696-701` (`FortranWordLists[]`). Test invariant
#2 pins pointer-equality of the two theme lookups
to catch a future copy-paste divergence — same
discipline as `L_JSON || L_JSON5`.

**Case-INSENSITIVE matching.** Fortran is
case-insensitive at the spec level (every standard
from FORTRAN 66 through Fortran 2023). LexFortran's
identifier classifier at `:167-179` calls
`sc.GetCurrentLowered(s, sizeof(s))` — the source
token is lowercased before every
`keywords.InList(s)` probe. Wordlist tokens must
therefore be all-lowercase. Test invariant #7
pins the all-lowercase contract; any uppercase
entry would silently never match.

**Three-class wordlist install:**
- **Class 0 → `SCE_F_WORD`** —
  `FORTRAN_KEYWORDS` carries **141 tokens**
  covering control flow (`if`/`do`/`select`/
  `case`/`where`/`forall`/`associate`),
  intrinsic types + type constructs
  (`integer`/`real`/`character`/`complex`/
  `logical`/`double`/`doubleprecision`/
  `precision`/`kind`/`len`/`type`/`class`),
  declaration modifiers, program units
  (`program`/`subroutine`/`function`/`module`/
  `submodule`/`interface`), attributes / OO
  (`public`/`private`/`recursive`/`pure`/
  `elemental`/`abstract`/`extends`/`deferred`),
  I/O statements (`open`/`read`/`write`/
  `inquire`), and F2008/F2018 additions
  (`critical`/`concurrent`/`event`/`team`/
  `fail`/`image`/`notify`/`sync`/`lock`/`unlock`).
  Rendered bold-blue.
- **Class 1 → `SCE_F_WORD2`** —
  `FORTRAN_INTRINSICS` carries **110 tokens**
  covering the pre-Fortran-95 stable core: 36 F77
  intrinsics, 72 F90 additions, 2 F95 additions
  (`cpu_time`, `null`). Elemental math (`abs`,
  `sqrt`, `sin`, `cos`, `tan`, `log`, `exp`,
  `dim`, `dprod`), complex-family accessors
  (`aimag`, `conjg`), elemental character
  (`adjustl`, `adjustr`, `len_trim`, `index`,
  `scan`, `verify`), elemental bit (`btest`,
  `iand`, `ior`, `ieor`, `not`, `ishft`), type
  conversion (`achar`, `char`, `cmplx`, `dble`,
  `int`), inquiry (`allocated`, `associated`,
  `present`, `shape`, `size`, `ubound`, `lbound`,
  `bit_size`, `digits`, `epsilon`, `huge`,
  `tiny`, `null`), transformational (`sum`,
  `product`, `matmul`, `dot_product`, `reshape`,
  `spread`, `pack`, `unpack`, `cshift`,
  `eoshift`, `transpose`, `merge`, `maxval`,
  `minval`, `transfer`), intrinsic subroutines
  (`cpu_time`, `date_and_time`, `mvbits`,
  `random_number`, `random_seed`,
  `system_clock`). Rendered accent-color.
- **Class 2 → `SCE_F_WORD3`** —
  `FORTRAN_EXTENDED` carries **55 tokens**
  covering F2003+ extensions: F2003 additions
  (`move_alloc`, `storage_size`,
  `execute_command_line`, `new_line`,
  `command_argument_count`,
  `get_command_argument`, `get_command`,
  `get_environment_variable`,
  `selected_char_kind`, `is_iostat_end`,
  `is_iostat_eor`), F2003 ISO_C_BINDING
  (`c_loc`, `c_funloc`, `c_associated`,
  `c_f_pointer`, `c_f_procpointer`, `c_sizeof`),
  F2008 bit intrinsics (`popcnt`, `poppar`,
  `leadz`, `trailz`, `shifta`, `shiftl`,
  `shiftr`, `dshiftl`, `dshiftr`, `maskl`,
  `maskr`, `merge_bits`), F2008 array
  (`findloc`, `bge`, `bgt`, `ble`, `blt`,
  `iall`, `iany`, `iparity`, `norm2`, `parity`,
  `is_contiguous`), F2008 coarray (`num_images`,
  `this_image`, `image_index`, `lcobound`,
  `ucobound`), F2018 collective subroutines
  (`co_broadcast`, `co_max`, `co_min`,
  `co_sum`, `co_reduce`), F2018 event/team
  intrinsics (`event_query`, `get_team`,
  `team_number`, `coshape`), F2018 array
  (`reduce`). Same accent-color slot as class 1
  — two intrinsic-function classes collapse to
  one visual identifier category.

**Style routing (13 mappings):** `COMMENT` (1) →
`Comment` (italic); `NUMBER` (2) → `Number`;
`STRING1` (3) + `STRING2` (4) + `STRINGEOL` (5)
→ `String` (three string flavours collapse:
`'...'`, `"..."`, unterminated-EOL); `OPERATOR`
(6) + `OPERATOR2` (12) → `Operator` (`.eq.` /
`.and.` / `.true.` etc. `.name.` forms share
punctuation colour); `WORD` (8) → `Keyword`
(bold); `WORD2` (9) + `WORD3` (10) → `Keyword2`
(intrinsics collapse); `PREPROCESSOR` (11) →
`Preprocessor` (compiler directives `!DEC$` /
`!DIR$` / `!MS$` + `#include` / `#define`);
`LABEL` (13) → `Keyword2` (statement labels are
branch targets); `CONTINUATION` (14) → `Operator`
(line-continuation marker). `DEFAULT` (0) and
`IDENTIFIER` (7) unmapped per framework
convention.

**Dual-role tokens deliberately in class 0 only.**
`kind`, `len`, `real`, `precision` are all
Fortran intrinsics AND type-parameter specifiers
(`INTEGER(KIND=8)`, `CHARACTER(LEN=10)`,
`REAL :: x`, `DOUBLE PRECISION`). LexFortran
probes class 0 first per `:171-176`, so listing
them in class 1 would be dead code. Test
invariant #16 pins the quartet in class 0 and
their absence from class 1; cross-list
uniqueness invariant #8 catches any future edit
that duplicates a token.

**`.name.` operator handling.** LexFortran routes
`.eq.` / `.and.` / `.not.` / `.true.` / `.false.`
into `SCE_F_OPERATOR2` via the `.` prefix
handler at `:244-245`. The dot-name-dot pattern
never reaches the wordlist probe. Invariant #17
asserts operator-word forms (`and`, `or`, `eq`,
`ne`, `true`, `false`, `lt`, `le`, `gt`, `ge`,
`eqv`, `neqv`) are absent from all three
wordlists. **`not` is the exception** — bare
`NOT(i)` is the F90 bit-manipulation intrinsic
and correctly hits class 1; the `.NOT.` operator
form is disambiguated by the surrounding dots.
Affirmative pin asserts `not` IS in
`FORTRAN_INTRINSICS`.

**Compound single-word `end<construct>` forms
included** — `endif`, `enddo`, `endsubroutine`,
`endfunction`, `endmodule`, `endsubmodule`,
`endinterface`, `endblock`, `endprocedure`,
`endtype`, `endwhere`, `endforall`,
`endassociate`, `endcritical`, `endenum`,
`endprogram`, `endselect`, `endblockdata`,
`endteam`. Also
`dowhile`, `selectcase`, `selecttype`,
`doubleprecision`, `blockdata`. These are legal
single-identifier tokens (no whitespace inside),
so LexFortran's identifier probe returns them as
one token. Notepad++/SciTE convention —
lighting up legacy code that writes them fused.

Structural test coverage: 17 invariants —
`Some(&FORTRAN_THEME)` return for BOTH `L_FORTRAN`
and `L_FORTRAN_77` (with pointer-equality pin
catching a copy-paste divergence), 13-mapping
style count, three-class canonical descriptor
order, all classes non-empty, Fortran identifier
alphabet enforcement, all-lowercase enforcement,
cross-list uniqueness across WL0/WL1/WL2,
style-routing pins for the 13 mapped SCE
constants, DEFAULT (0) + IDENTIFIER (7) unmapped,
italic set == 1 (COMMENT only), bold set == 1
(WORD only), cross-language non-reuse against R
/ CoffeeScript / JSON / D, `L_FORTRAN`
`LangEntry`'s `lexer: Some("fortran")` + `.f90`
extension AND `L_FORTRAN_77` `LangEntry`'s
`lexer: Some("f77")` + `.f` extension, canonical
anchor coverage (WL0 primary keywords, WL1
canonical intrinsics, WL2 F2003+ extended), the
`kind`/`len`/`real`/`precision` dual-role quartet
in class 0 only with affirmative
class-1-absence pins, and operator-word `.name.`
form absence pins (with the `not` exception
documented and affirmatively pinned).

**CSound (2026-07-05):** wires `SCLEX_CSOUND`
(= 74) for Csound orchestra (`.orc`), score
(`.sco`), and unified `.csd` sources. `L_CSOUND`
(id 70) is the sole language row using this
lexer — no shared-theme dispatch needed.

**Rate-prefix auto-classification is Csound's
signature.** `LexCsound.cxx:101-111` examines the
first character of any identifier that fails all
three wordlist probes and routes it to a
rate-typed variable state — `p` → `PARAM`, `a`
→ `ARATE_VAR`, `k` → `KRATE_VAR`, `i` →
`IRATE_VAR`, `g` → `GLOBAL_VAR`. Every Csound
variable carries its evaluation rate in the name
(`aOut` is audio-rate, `kEnv` is control-rate,
`iFreq` is init-rate, `gaMaster` is a global
audio-rate variable, `p4`/`p5` are instrument
parameters). The theme collapses the four
rate-var states + HEADERSTMT + USERKEYWORD to
`Keyword2` — six visual identifier categories
share one accent slot. `PARAM` gets a distinct
`Preprocessor` slot: instrument-parameter
references are structural inputs.

**Three-class wordlist (~365 + 28 + 25 = ~418
tokens):**
- **Class 0 (`SCE_CSOUND_OPCODE`, bold Keyword)**
  — `CSOUND_OPCODES` carries ~365 canonical
  Csound opcodes across 15 semantic categories,
  including `instr` / `endin` (moved here from
  class 1 so `FoldCsoundInstruments` at
  `LexCsound.cxx:170-183` can fold instrument
  blocks — the fold classifier's positive trigger
  at `:170` requires `SCE_CSOUND_OPCODE` styling):
  oscillators (`oscil`/`vco`/`buzz`), physical
  models (`pluck`/`wgbow`/`fmvoice`), envelope
  generators (`linen`/`adsr`/`transeg`), filters
  (`butlp`/`moogladder`/`reson`), reverbs +
  effects (`reverb`/`freeverb`/`chorus`/`delay`),
  I/O (`out`/`in`/`monitor`), math intrinsics
  (`abs`/`sqrt`/`sin`/`log`), conversion +
  amplitude (`ampdb`/`cpspch`/`cpsmidi`), random
  + noise (`rand`/`noise`/`gauss`), function
  tables (`table`/`ftgen`/`diskin`), string +
  print (`printf`/`sprintf`/`strcat`), MIDI
  (`midiin`/`ampmidi`/`ctrl7`), signal + event
  control (`chnget`/`schedule`/`event`),
  spectral PVS suite (`pvsanal`/`pvsynth`/24
  siblings), granular (`grain`/`fof`/
  `syncgrain`).
- **Class 1 (`SCE_CSOUND_HEADERSTMT`, accent
  Keyword2)** — `CSOUND_HEADERSTMT` carries 28
  orchestra/score/preprocessor tokens: 6 global
  config settings (`sr`/`kr`/`ksmps`/`nchnls`/
  `nchnls_i`/`0dbfs`), 2 user-opcode block markers
  (`opcode`/`endop` — `instr`/`endin` moved to
  class 0 for the fold classifier), 15
  single-letter score statements (`f`/`i`/`a`/`t`/
  `b`/`e`/`s`/`v`/`n`/`x`/`q`/`r`/`m`/`y`/`d`), 5
  bare preprocessor forms (`include`/`define`/
  `undef`/`ifdef`/`ifndef`).
- **Class 2 (`SCE_CSOUND_USERKEYWORD`, accent
  Keyword2)** — `CSOUND_USERKW` carries 25
  control-flow tokens: conditionals (`if`/
  `then`/`else`/`elseif`/`endif`), loops
  (`while`/`until`/`do`/`od`), unconditional
  gotos (`goto`/`igoto`/`kgoto`/`tigoto`/
  `timout`), conditional gotos (`cggoto`/`cigoto`/
  `ckgoto`/`cngoto`), counted-loop opcodes
  (`loop_ge`/`loop_gt`/`loop_le`/`loop_lt`),
  subroutine + reinit control (`return`/`reinit`/
  `rireturn`).

**Semantic rationale for class-2 control-flow
placement.** The Csound manual formally
documents `if`/`then`/`else`/`goto` etc. as
"opcodes" (they appear in the opcodes manual
index), but that's a documentation-grouping
choice — these emit no signal and read as
syntactic execution-flow control, not
audio-processing primitives. Scintilla lexers
for comparable languages routinely split control
words into their own style slot. Placing them
in class 2 gives them accent color (Keyword2)
instead of bold (Keyword), matching how
control-flow-aware Csound grammars render them.
Test invariant #15 pins the 16 control-flow
tokens in class 2 with affirmative class-0
absence to catch a future regression that
copy-pastes them into class 0.

**Style routing (11 mappings):** COMMENT →
`Comment` (italic); NUMBER → `Number`; OPERATOR
→ `Operator`; OPCODE → `Keyword` (bold);
HEADERSTMT + USERKEYWORD + ARATE_VAR + KRATE_VAR
+ IRATE_VAR + GLOBAL_VAR → `Keyword2` (six
categories collapse); PARAM → `Preprocessor`
(structural distinction). DEFAULT (0) +
IDENTIFIER (5) unmapped per framework convention.

**Three orphan enum slots** (INSTR (4),
COMMENTBLOCK (9), STRINGEOL (15)) defined in
`SciLexer.h` but never entered by
`ColouriseCsoundDoc`. INSTR is a legacy slot;
COMMENTBLOCK signals that the lexer has no
`/* */` block-comment handling (only `;`-to-EOL);
STRINGEOL is only referenced as an `initStyle`
guard at `LexCsound.cxx:63-64`. Test invariant
#9 pins the deliberate non-inclusion.

**No string handling** — LexCsound's paint loop
at `:68-152` never enters a string state. Quote
characters `"`/`'` are not in `IsCsoundOperator`
or `IsAWordStart`, so quoted strings remain in
DEFAULT. Ubiquitous in Csound (`"filename.wav"`
in `diskin`, format strings in `printf`) but
correctly reflects the upstream lexer's
capabilities.

**`0dbfs` starts with a digit** so `LexCsound.cxx:132`
routes it to NUMBER state before IDENTIFIER,
meaning the wordlist probe never runs for this
token. Kept in the class-1 wordlist for
completeness in case a future lexer change adds
number-vs-identifier disambiguation, but
currently dead code for this specific token.

Structural test coverage: 16 invariants —
`Some(&CSOUND_THEME)` return, 11-mapping style
count, three-class canonical descriptor order,
all classes non-empty, Csound identifier
alphabet enforcement `[a-z0-9_]+` (single-letter
score statements permitted), cross-list
uniqueness across WL0/WL1/WL2, style-routing
pins for the 11 mapped SCE constants, DEFAULT +
IDENTIFIER + INSTR + COMMENTBLOCK + STRINGEOL
all unmapped, italic set == 1 (COMMENT only),
bold set == 1 (OPCODE only), cross-language
non-reuse against R / CoffeeScript / Fortran /
JSON, `L_CSOUND` `LangEntry`'s `lexer:
Some("csound")` + `.orc` + `.sco` + `.csd`
extension presence, canonical anchor coverage
(WL0 opcodes / WL1 headers / WL2 control-flow —
including affirmative pins that `instr` / `endin`
live in WL0 for the fold classifier and are NOT
in WL1), 25 class-2 control-flow keywords in
class 2 only with affirmative class-0 absence
(plus an affirmative absence pin for the
fabricated `enduntil` token), and 15 single-
letter score statements in class 1.

**Erlang (2026-07-05):** wires `SCLEX_ERLANG`
(= 53) for Erlang source (`.erl`) and header
(`.hrl`) files. `L_ERLANG` (id 71) is the sole
language row using this lexer — no shared-theme
dispatch needed.

**Six-class wordlist descriptor is the widest of
Phase 4.5 so far.** `erlangWordListDesc[]` at
`LexErlang.cxx:616-624` splits reserved words /
BIFs / preprocessor / module-attrs / doc / doc-macro
into six independent lists. Two of the six carry
a sigil prefix on every token (`-define`/`-module`
in classes 2 + 3, `@doc`/`@link` in classes 4 +
5) because the paint loop captures the sigil at
state entry via `SetState(SCE_ERLANG_UNKNOWN)` at
`:480-481` (for `-`) and `ForwardSetState` at
`:143` (for `@`) — `sc.GetCurrent(cur, sizeof(cur))`
then returns the buffer starting with the sigil,
so wordlist entries must include it.

**Fold classifier is spelling-literal.**
`FoldErlangDoc` at `:531-614` and
`ClassifyErlangFoldPoint` at `:508-529` match six
specific token spellings directly via
`styler.Match(keyword_start, "case"/"fun"/"if"/
"query"/"receive"/"end")` — after the guard
`stylePrev != SCE_ERLANG_KEYWORD && style ==
SCE_ERLANG_KEYWORD` at `:558-559`. Load-bearing
consequence: if any of `case`, `fun`, `if`,
`query`, `receive`, or `end` is missing from
`ERLANG_KEYWORDS`, its identifier settles to ATOM
(or FUNCTION_NAME) instead of KEYWORD, the fold
guard misses, and Erlang blocks won't fold at
`case ... end` / `if ... end` / `fun ... end` /
`receive ... end` boundaries. Test invariant #15
pins all six affirmatively.

**Six-class wordlist (30 + 131 + 12 + 24 + 21 + 10
= 228 tokens):**
- **Class 0 (`SCE_ERLANG_KEYWORD`, bold Keyword)**
  — `ERLANG_KEYWORDS` carries all 30 Erlang OTP
  reserved words: bitwise/boolean operators
  (`and`/`or`/`not`/`xor`/`andalso`/`orelse`/
  `band`/`bor`/`bxor`/`bnot`/`bsl`/`bsr`), block
  openers/closers (`case`/`catch`/`fun`/`if`/
  `receive`/`try`/`end`/`after`/`when`), the
  obsolete `query`/`cond`/`let` triple (kept for
  fold-classifier and lexer-tolerance reasons), and
  OTP 25+ `maybe`/`else` for the `maybe ... else`
  construct.
- **Class 1 (`SCE_ERLANG_BIFS`, bold Keyword)** —
  `ERLANG_BIFS` carries 131 BIFs from the `erlang`
  module (auto-imported forms callable without the
  `erlang:` prefix — `spawn`, `is_atom`,
  `list_to_binary`, ... — plus commonly-used
  prefixed forms `erlang:system_info` /
  `erlang:send_after` / `erlang:phash2` /
  `erlang:process_display` /
  `erlang:unique_integer` for coverage of both
  idiomatic call-site shapes): type
  predicates (`is_atom`/`is_binary`/`is_list`/16
  siblings), type conversions (29 `X_to_Y`
  functions), size accessors (`length`/`bit_size`/
  `byte_size`/`map_size`/`tuple_size`), math
  intrinsics (`abs`/`ceil`/`floor`/`round`/
  `trunc`/`max`/`min`/`float`), process control
  (`spawn` family / `link`/`unlink`/`monitor`/
  `demonitor`/`register`/`unregister`/`whereis`/
  `self`/`node`/`nodes`/`exit`/`halt`/
  `process_flag`/`process_info`), messaging
  (`send`/`send_after`/`send_nosuspend`), term
  manipulation (`apply`/`error`/`throw`/`get`/
  `put`/`erase`/`element`/`setelement`/`make_ref`/
  `date`/`time`/`statistics`/`memory`/
  `system_info`/`unique_integer`/`phash2`), code
  loading (`load_module`/`purge_module`/
  `check_old_code`/`garbage_collect`), binaries
  (`binary_part`/`split_binary`), ports
  (`open_port`/`port_close`/`port_command`), list
  head/tail (`hd`/`tl`).
- **Class 2 (`SCE_ERLANG_PREPROC`, Preprocessor
  slot)** — `ERLANG_PREPROC` carries 12
  preprocessor directives, all `-`-prefixed:
  conditional compilation (`-define`/`-undef`/
  `-ifdef`/`-ifndef`/`-if`/`-elif`/`-else`/
  `-endif`), file inclusion (`-include`/
  `-include_lib`), compile-time diagnostics
  (`-error`/`-warning`). `-elif` was added in OTP
  26 (May 2023).
- **Class 3 (`SCE_ERLANG_MODULES_ATT`,
  Preprocessor slot)** — `ERLANG_MODULE_ATT`
  carries 24 module attributes, all `-`-prefixed:
  structural (`-module`/`-export`/`-import`/
  `-export_type`/`-on_load`/`-nifs`), behavior
  declarations (`-behaviour`/`-behavior` — both
  spellings accepted / `-callback`/
  `-optional_callbacks`), type specifications
  (`-spec`/`-type`/`-opaque`), records
  (`-record`), metadata (`-vsn`/`-author`/
  `-copyright`/`-deprecated`/`-removed`), compile
  control (`-compile`/`-dialyzer`/`-feature`), OTP
  27+ documentation attributes (`-doc`/
  `-moduledoc`).
- **Class 4 (`SCE_ERLANG_COMMENT_DOC`, Comment
  slot with italic)** — `ERLANG_DOC` carries 21
  edoc tags, all `@`-prefixed: authorship
  (`@author`/`@copyright`/`@version`/`@since`),
  documentation structure (`@doc`/`@docfile`/
  `@end`/`@equiv`/`@headerfile`/`@hidden`/
  `@private`/`@todo`/`@TODO`/`@deprecated`),
  function signatures (`@param`/`@spec`/
  `@returns`/`@throws`/`@type`), cross-references
  (`@reference`/`@see`). `@todo` and `@TODO` are
  distinct per edoc case-sensitivity.
- **Class 5 (`SCE_ERLANG_COMMENT_DOC_MACRO`,
  Comment slot with italic)** — `ERLANG_DOC_MACRO`
  carries 10 inline edoc `{@…}` macros:
  `@link`/`@module`/`@section`/`@title`/`@type`/
  `@version`/`@time`/`@date`/`@email`/`@url`.
  Overlap with class 4 (`@type`, `@version`) is
  deliberate: the two parse states are mutually
  exclusive — class 5 only fires when `parse_state
  == COMMENT_DOC_MACRO` at `:163-164`, class 4
  only when the tag appears bare. `@moduledoc`
  deliberately excluded — that token belongs to
  OTP 27+'s `-moduledoc` module attribute (class
  3), not the edoc `{@…}` inline macro set.

**Style routing (23 mappings across 26 defined
SCE slots):** COMMENT + COMMENT_FUNCTION +
COMMENT_MODULE + COMMENT_DOC + COMMENT_DOC_MACRO
→ `Comment` (italic — five comment levels
collapse; the `%`/`%%`/`%%%` ratchet and embedded
edoc variants all read as comments); VARIABLE +
FUNCTION_NAME + RECORD + RECORD_QUOTED + NODE_NAME
+ NODE_NAME_QUOTED + MODULES → `Keyword2` (seven
structural-marker categories collapse to accent
color); NUMBER → `Number`; KEYWORD + BIFS →
`Keyword` (bold — two reserved-vocabulary classes
share the primary slot); STRING + CHARACTER +
ATOM_QUOTED → `String` (three quoted-literal
forms collapse); OPERATOR → `Operator`; MACRO +
MACRO_QUOTED → `Macro` (matches Rust's Macro
slot usage — `?MACRO` reads as a preprocessor-style
invocation, visually distinct from records `#name`
and module attributes `-name`); PREPROC +
MODULES_ATT → `Preprocessor` (both `-`-prefixed,
share the compiler-directive family). DEFAULT (0)
+ ATOM (7) + UNKNOWN (31) unmapped per framework
convention — ATOM is Erlang's bare
lowercase-first identifier form (the most common
token), left at STYLE_DEFAULT so atoms paint as
regular text and the accent goes only to
genuinely marked identifiers.

**Multi-level comment ratchet** is unique to
LexErlang: `%` opens COMMENT, a second `%` on the
same line ratchets to COMMENT_FUNCTION at
`:112-117`, a third ratchets to COMMENT_MODULE at
`:125-130`. The `to_late_to_comment` flag at
`:111,124` prevents downgrading if a non-`%`
character intervened. Every ratchet level remains
subject to embedded edoc `@tag`/`{@macro}`
detection at `:136-153` for a nested doc emit
that overrides the outer comment style.

**Sigil-carrying wordlist entries are the
signature complication.** Four of the six classes
require the sigil verbatim; test invariants #6
(preproc/module-att `-` prefix) and #7
(doc/doc-macro `@` prefix) enforce this. A future
contributor tempted to add a bare `include` to
`ERLANG_PREPROC` would zero-match — the paint
loop compares `-include` (with leading `-`)
against the wordlist, so bare `include` would
never fire. Invariant #20 also pins the
converse: bare-identifier classes (KEYWORDS +
BIFS) must NOT carry sigils, because the
atom-classification path at `:213-217` only sees
bare identifiers stripped of any sigil context.

Structural test coverage: 20 invariants —
`Some(&ERLANG_THEME)` return, 23-mapping style
count, six-class canonical descriptor order, all
classes non-empty, bare-identifier alphabet
enforcement for classes 0 + 1 (`[a-z0-9_]+`),
`-` prefix enforcement for classes 2 + 3, `@`
prefix enforcement for classes 4 + 5, cross-list
uniqueness for the two same-parse-state pairs
(KEYWORDS ∩ BIFS = ∅; PREPROC ∩ MODULE_ATT = ∅;
DOC ∩ DOC_MACRO is deliberately allowed to
overlap because they fire in different parse
states), style-routing pins for all 23 mapped SCE
constants, DEFAULT + ATOM + UNKNOWN unmapped,
italic set == 5 (all comment states), bold set
== 2 (KEYWORD + BIFS), cross-language non-reuse
against CSound / CoffeeScript / Fortran / JSON,
`L_ERLANG` `LangEntry`'s `lexer: Some("erlang")`
+ `.erl` + `.hrl` extension presence,
fold-classifier tokens (`case`/`fun`/`if`/
`query`/`receive`/`end`) affirmatively in class
0, modern-Erlang OTP-25+ anchors (`maybe`/
`else`), canonical BIF anchor coverage
(`spawn`/`is_atom`/`list_to_binary`/
`binary_to_term`/`self`/`apply`), sigil-carrying
canonical anchors (`-define` in PREPROC,
`-module` in MODULE_ATT), doc-tag anchors
(`@doc` in DOC, `@link` in DOC_MACRO), and
affirmative absence of `-`/`@` sigils in classes
0 + 1.

**ESCRIPT (2026-07-05):** wires `SCLEX_ESCRIPT`
(= 41) for POL (Penultima Online) server-side
scripting language `.em` sources. `L_ESCRIPT`
(id 72) is the sole language row using this
lexer.

**Semantic-label mismatch is load-bearing.**
`LexEScript`'s three-class descriptor at
`LexEScript.cxx:270-275` labels class 2 as
"Extended and user defined functions" — but the
fold classifier `FoldESCRIPTDoc` at
`LexEScript.cxx:232-243` only fires on tokens
styled as `SCE_ESCRIPT_WORD3` (class 2 hit),
`classifyFoldPointESCRIPT` at `:152-171`
hard-codes 16 specific fold-critical control-flow
spellings via `strcmp`. This forces the language's
fold-critical keywords (`for`/`foreach`/`program`/
`function`/`while`/`case`/`if` openers; the seven
`end*` closers; `else`/`elseif` half-block markers)
into class 2 — the semantic opposite of what the
descriptor's label suggests. The theme compensates
by routing `SCE_ESCRIPT_WORD3` → `Keyword` (bold,
matching the semantic weight of language
keywords), NOT `Keyword2` accent. This mirrors
Erlang's KEYWORD + BIFS → Keyword collapse
discipline: two related classes can share the
primary bold slot when both semantically read as
"language vocabulary".

**First-match-wins cascade is a landmine.** The
identifier classifier at `LexEScript.cxx:92-97`
probes class 0 → class 1 → class 2 in order. A
fold-critical token duplicated in class 0 gets
`SCE_ESCRIPT_WORD` (bold Keyword) instead of
`SCE_ESCRIPT_WORD3`, and the fold classifier
never sees it — silently breaking fold at that
block boundary. Test invariant #13 pins
cross-list disjointness affirmatively (both
class-2 presence AND class-0 + class-1 absence
for every one of the 16 fold-classifier tokens);
test invariant #6 additionally pins any
pairwise intersection across the three wordlists.

**Three-class wordlist (27 + 50 + 16 = 93
tokens):**
- **Class 0 (`SCE_ESCRIPT_WORD`, bold Keyword)** —
  `ESCRIPT_KEYWORDS` carries 27 non-fold primary
  vocabulary tokens: declarations (`var`/`const`/
  `dictionary`/`struct`/`enum`), module control
  (`use`/`include`), boolean literals + nil
  (`true`/`false`/`nil`), Pascal-style boolean and
  type-check word operators (`and`/`or`/`not`/
  `isa` — `isa` is POL's `obj isa POLCLASS_XXX`
  type-check operator, analogous to Delphi's `is`),
  control-flow exits (`return`/`break`/`continue`/
  `exit`), iteration modifiers (`do`/`then`/`to`/
  `downto`/`step`/`in`), and non-fold-recognised
  loop constructs (`repeat`/`until`/`goto`).
- **Class 1 (`SCE_ESCRIPT_WORD2`, Keyword2 accent)**
  — `ESCRIPT_INTRINSICS` carries 50 canonical POL
  intrinsic functions across three modules:
  `basic.em` (`print`/`println`/`syslog`/`cint`/
  `cdbl`/`cstr`/`len`/`typeof`/`randomint`/`sqrt`/
  `sleep`/`substr`/`strreplace` etc.), `uo.em`
  (`sendsysmessage`/`findplayer`/
  `createitematlocation`/`movecharacter`/`getx`/
  `gety`/`getz`/`getobjproperty` etc.), and
  `os.em` (`start_script`/`run_script`/
  `system_time`/`readmillisecondclock`/
  `set_critical`/`wait_for_event`).
- **Class 2 (`SCE_ESCRIPT_WORD3`, bold Keyword —
  fold-critical)** — `ESCRIPT_FOLDWORDS` carries
  exactly the 16 spellings hard-coded in
  `classifyFoldPointESCRIPT` at `:152-171`:
  block openers (`for`/`foreach`/`program`/
  `function`/`while`/`case`/`if`), block closers
  (`endfor`/`endforeach`/`endprogram`/
  `endfunction`/`endwhile`/`endcase`/`endif`),
  half-block markers (`else`/`elseif`).

**Style routing (10 mappings across 12 defined SCE
slots):** COMMENT + COMMENTLINE + COMMENTDOC →
`Comment` (italic — three comment forms collapse;
COMMENTDOC is a currently-orphan enum slot that
`ColouriseESCRIPTDoc` never enters but mapped
anyway for forward-compat); NUMBER → `Number`;
WORD + WORD3 → `Keyword` (bold — two "language
keyword" classes collapse despite class 2's
misleading descriptor label); STRING → `String`;
OPERATOR + BRACE → `Operator` (two punctuation
classes collapse — LexEScript splits `{ }` from
other operators but semantically they're all
punctuation); WORD2 → `Keyword2` (accent for
intrinsic functions). DEFAULT (0) + IDENTIFIER (8)
unmapped per framework convention.

**Case-INSENSITIVE by default** via the
`escript.case.sensitive` property (default 0) at
`LexEScript.cxx:54`. When unset, the identifier
classifier at `:87` calls `sc.GetCurrentLowered`
before the wordlist probe — so all three
wordlists must be all-lowercase. Invariant #5
enforces `[a-z0-9_]+` alphabet across every
wordlist.

**Restricted operator set** at
`LexEScript.cxx:140` — the paint loop explicitly
enumerates `+ - * / = < > & | ! ? :` rather than
using Scintilla's `isoperator`, so `. , ; ( ) [ ]`
render as `SCE_ESCRIPT_DEFAULT` (unstyled). This
is a known LexEScript limitation that this
wiring inherits.

Structural test coverage: 16 invariants —
deep-value identity pin, 10-mapping style count,
three-class canonical descriptor order, all
classes non-empty, all-lowercase alphabet
enforcement `[a-z0-9_]+`, cross-list disjointness
across all three pairs (KEYWORDS ∩ INTRINSICS,
KEYWORDS ∩ FOLDWORDS, INTRINSICS ∩ FOLDWORDS —
load-bearing for fold correctness),
style-routing pins for all 10 mapped SCE
constants, DEFAULT + IDENTIFIER unmapped, italic
set == 3 (all comment states), bold set == 2
(WORD + WORD3 — the two language-keyword
classes), cross-language non-reuse against
Erlang / CSound / Fortran / JSON, `L_ESCRIPT`
`LangEntry`'s `lexer: Some("escript")` + `.em`
extension presence, exhaustive class-2 presence
+ class-0 + class-1 absence pin for all 16
fold-classifier tokens (`for`/`foreach`/
`program`/`function`/`while`/`case`/`if`/
`endfor`/`endforeach`/`endprogram`/
`endfunction`/`endwhile`/`endcase`/`endif`/
`else`/`elseif`), canonical non-fold anchors
(`var`/`const`/`return`/`true`/`false`/`nil`),
canonical intrinsic anchors (`print`/
`sendsysmessage`/`createitematlocation`/
`sleep` — one from each module), and no-duplicate
defence-in-depth check across all three
wordlists.

**Forth (2026-07-06):** wires `SCLEX_FORTH` (= 52)
for Forth `.forth` source. `L_FORTH` (id 73) is
the sole language row using this lexer. Widest
wordlist descriptor of Phase 4.5 (tied with
Erlang) — six independent classes.

**Six-class wordlist (25 + 206 + 18 + 15 + 2 + 6 =
272 tokens):**
- **Class 0 (`SCE_FORTH_CONTROL`, bold Keyword)** —
  `FORTH_CONTROL` carries 25 control-flow
  structural words: `if`/`else`/`then`/`endif`
  (conditionals), `begin`/`until`/`while`/
  `repeat`/`again` (indefinite loops), `do`/`?do`/
  `loop`/`+loop`/`leave`/`unloop` (counted loops),
  `case`/`of`/`endof`/`endcase` (case-select),
  `exit`/`quit`/`recurse` (definition-level
  control), `[if]`/`[else]`/`[then]` (Forth-2012
  TOOLS-EXT compile-time bracket conditionals).
- **Class 1 (`SCE_FORTH_KEYWORD`, bold Keyword)** —
  `FORTH_KEYWORD` carries 206 general runtime
  vocabulary tokens across ANS Forth CORE +
  CORE-EXT + basic FLOAT + STRING + MEMORY +
  TOOLS: stack manipulation (`dup`/`drop`/`swap`/
  return-stack `>r`/`r>`/loop indices `i`/`j`),
  single/double/mixed arithmetic (`+`/`-`/`*`/`/`
  through `um*`/`um/mod`/`m*`/`sm/rem`), comparison
  (`=`/`<>`/`<`/`>`/`0=`/`0<>`/`within`), logic
  (`and`/`or`/`xor`/`not`/`invert`), memory (`@`/
  `!`/`c@`/`c!`/`+!`/`move`/`fill`), base &
  pictured numeric output, I/O (`emit`/`type`/
  `cr`/`.`/`.r`), dictionary primitives (`here`/
  `allot`/`,`/`c,`/`align`), compile-time helpers
  that don't parse names (`literal`/`compile,`/
  `state`/`[`/`]`), search-order primitives
  (`also`/`previous`/`only`/`definitions`),
  string operations (`count`/`compare`/`search`),
  parsing accessors (`source`/`parse`/`>in`),
  exception (`abort`/`throw`/`catch`/`bye`),
  truth values (`true`/`false`), and a basic
  FLOAT set (`f+`/`f-`/`fdup`/`f@`/`fsqrt`/etc.).
- **Class 2 (`SCE_FORTH_DEFWORD`, Keyword2 accent)**
  — `FORTH_DEFWORD` carries 18 definition words:
  `variable`/`constant`/`value` (and their double/
  float variants), `create`/`does>`/`defer`,
  attribute markers `immediate`/`compile-only`/
  `recursive`, Forth-2012 `buffer:`, vocabulary
  primitives `vocabulary`/`wordlist`. `:` and `;`
  are DELIBERATELY EXCLUDED — the paint loop at
  `LexForth.cxx:138-149` auto-styles these two
  chars as `SCE_FORTH_DEFWORD` without wordlist
  lookup, so an entry here would be dead code.
- **Class 3 (`SCE_FORTH_PREWORD1`, Preprocessor)** —
  `FORTH_PREWORD1` carries 15 compile-time / runtime
  words that consume the next single token from
  the input stream: `postpone`/`[']`/`[char]`/`'`
  (compile-time name-parsers), `char`/`see`
  (runtime name-parsers), `to`/`is` (value/defer
  assignment), `include`/`?include`/`require`/
  `needs` (file inclusion — name-parsing forms
  only; `include-file` moved to `FORTH_KEYWORD`
  since Forth-2012 §11.6.1.1717 defines it with
  stack signature `( fileid -- )`),
  `[defined]`/`[undefined]` (compile-time
  predicates), `marker`.
- **Class 4 (`SCE_FORTH_PREWORD2`, Preprocessor)** —
  `FORTH_PREWORD2` carries exactly 2 tokens
  covering the niche "2-argument preword"
  category: `synonym` (Forth-2012 §15.6.2.2525
  TOOLS-EXT — `SYNONYM new-name old-name`) and
  `alias` (Gforth/ISO Forth systems — same
  2-word signature). Test invariant #18 pins
  the cardinality-2 to prevent future
  fabrication.
- **Class 5 (`SCE_FORTH_STRING`, String)** —
  `FORTH_STRINGS` carries 6 string-parsing openers:
  `s"` (§6.1.2165), `."` (§6.1.0190), `abort"`
  (§6.1.0680), `c"` (§6.2.0855), `s\"` (Forth-2012
  §11.6.1.2165.35), `z"` (Gforth/SwiftForth/iForth
  null-terminated). **Every entry MUST end in `"`**
  — behaviourally load-bearing at
  `LexForth.cxx:86-87` and :98-101, invariant #19
  pins this affirmatively.

**Style routing (10 mappings across 12 defined
SCE slots):** COMMENT + COMMENT_ML → `Comment`
(italic — `\` line comment + `( ... )` block
comment collapse); CONTROL + KEYWORD → `Keyword`
(bold — two "language vocabulary" classes
semantically collapsed, same discipline as
Erlang KEYWORD + BIFS or ESCRIPT WORD + WORD3);
DEFWORD → `Keyword2` (accent for definition-word
events); PREWORD1 + PREWORD2 → `Preprocessor`
(two tiers of compile-time next-token consumers
collapse); NUMBER → `Number`; STRING → `String`;
LOCALE → `Keyword2` (Forth-2012 `{ name1 name2
... }` locals styled as a lightweight
definition form). DEFAULT (0) + IDENTIFIER (3)
unmapped per framework convention.

**Case-INSENSITIVE by design.** Forth is
traditionally written in uppercase but the
lexer's `GetCurrentLowered` at
`LexForth.cxx:73` lowercases the source before
wordlist probing. All 272 wordlist tokens are
lowercase.

**First-match-wins cascade at :75-88** across
all six classes in class order 0 → 5. A token
duplicated in an earlier class silently masks
its later-class sibling. Test invariant #6
pins pairwise cross-class disjointness across
all 15 class-pair combinations —
load-bearing for correct styling.

**No fold.** `FoldForthDoc` at `:157-159` is a
no-op stub. Forth's whitespace-delimited
nested-parenthesis grammar doesn't admit
line-based folding.

**Symbolic word alphabet.** `IsAWordStart` at
`:31-35` accepts alnum + `!#'()*+,-./<=>?@[\]_`,
and the identifier-continuation is
IsASpaceChar-only at `:71` — meaning any
non-whitespace can extend a token. Consequence:
tokens like `>r` / `+!` / `,` / `@` / `buffer:`
/ `s"` are all valid single-word identifiers.
Test invariant #5 accepts this full alphabet.

Structural test coverage: 20 invariants —
deep-value identity pin, 10-mapping style
count, six-class canonical descriptor order,
all classes non-empty, all-lowercase Forth-word
alphabet enforcement across every class,
**pairwise cross-class disjointness across
all 15 class-pair combinations**
(load-bearing for the first-match-wins
cascade), style-routing pins for all 10
mapped SCE constants, DEFAULT + IDENTIFIER
unmapped, italic set == 2 (both comment
states), bold set == 2 (CONTROL + KEYWORD),
cross-language non-reuse against Erlang /
CSound / Fortran / ESCRIPT, `L_FORTH`
`LangEntry`'s `lexer: Some("forth")` +
`forth` extension presence, affirmative
absence pin for `:` and `;` from DEFWORD
(auto-styled by paint loop, wordlist entry
dead code), canonical control anchors (`if`/
`else`/`then`/`begin`/`do`/`case`/`recurse`/
`[if]` — one from each sub-family), canonical
keyword anchors spanning stack/memory/
arithmetic/IO/loop-indices/FLOAT/booleans,
canonical defword/preword1 anchors, class-4
cardinality pin (exactly `synonym`/`alias`),
**all STRINGS tokens end in `"`** (load-bearing
for STRING-state entry/exit correctness), and
no-duplicate defence-in-depth check across
all six wordlists.

**MMIXAL (2026-07-06):** wires `SCLEX_MMIXAL`
(= 44) for MMIXAL `.mms` source — Donald
Knuth's MMIX assembly language from *The Art
of Computer Programming* Vol 1 Fascicle 1.
`L_MMIXAL` (id 75) is the sole language row
using this lexer. Same three-class descriptor
count as CSound and ESCRIPT.

**Three-class wordlist (239 + 32 + 28 = 299
tokens):**
- **Class 0 (`SCE_MMIXAL_OPCODE_VALID`, bold
  Keyword)** — `MMIXAL_OPCODES` carries the
  MMIX 256-opcode table's mnemonic surface
  plus 10 assembler pseudo-ops. Structured by
  functional family: 15 floating-point
  (`FADD`/`FSUB`/`FMUL`/…/`FSQRT`), 16 integer
  arithmetic (base + `-I` immediate:
  `MUL`/`MULI`/…/`SUB`/`SUBUI`), 16 scaled-add
  + compare + negate (`2ADDU`/…/`16ADDUI`/
  `CMP`/`NEG`), 8 shifts, 16 branches (source-
  level base only — `BN`/`BZ`/…/`PBEV`; the
  `-B` fwd/back suffix is byte-encoding-level,
  handled by the assembler), 32 conditional-
  set / zero-or-set (`CSN`/`CSNI`/…/`ZSEV`/
  `ZSEVI`), 24 loads (`LDB`/…/`CSWAP`/
  `LDUNC`), 8 load-associated + GO
  (`LDVTS`/`PRELD`/`PREGO`/`GO`), 24 stores
  (`STB`/…/`STCO`/`STUNC`), 8 store-associated
  + PUSHGO (`SYNCD`/`PREST`/`SYNCID`/`PUSHGO`),
  32 bitwise / byte-wise-difference /
  multiplex (`OR`/…/`MXOR`), 16 set/increment
  high/low + byte-wise or/andn (`SETH`/…/
  `ANDNL`), 5 jump/call/stack (`JMP`/`PUSHJ`/
  `GETA`/`PUT`/`POP`), 8 system/privileged
  (`RESUME`/`SAVE`/`UNSAVE`/`SYNC`/`SWYM`/
  `GET`/`TRAP`/`TRIP`), 1 immediate-form
  privileged (`PUTI` — the only opcode in the
  0xF0–0xFF group with a distinct immediate
  byte pair), 10 assembler
  pseudo-ops (`BYTE`/`WYDE`/`TETRA`/`OCTA`/
  `LOC`/`GREG`/`PREFIX`/`BSPEC`/`ESPEC`/`IS`).
  **Digit-prefix mnemonics** `2ADDU`/`4ADDU`/
  `8ADDU`/`16ADDU` (and immediates) present
  verbatim — the OPCODE_PRE transition at
  `LexMMIXAL.cxx:117-119` fires on any
  non-space (not `IsAWordStart`-restricted),
  so the digit-first mnemonic is captured
  and probed byte-exact.
- **Class 1 (`SCE_MMIXAL_REGISTER`, Keyword2
  accent)** — `MMIXAL_SPECIAL_REGISTERS`
  carries the 32 MMIX special registers per
  MMIXware Vol 1 §1.4: 26 primary (`rA`
  through `rZ`) + 6 shadow (`rBB`/`rTT`/
  `rWW`/`rXX`/`rYY`/`rZZ`) used on privileged-
  mode interrupt saves.
- **Class 2 (`SCE_MMIXAL_SYMBOL`, Keyword2
  accent)** — `MMIXAL_PREDEF_SYMBOLS` carries
  28 predefined MMIXAL identifiers: `Inf` (FP
  constant), 5 rounding modes (`ROUND_CURRENT`/
  `ROUND_OFF`/`ROUND_UP`/`ROUND_DOWN`/
  `ROUND_NEAR`), 3 memory-segment origins
  (`Data_Segment`/`Pool_Segment`/
  `Stack_Segment`), 11 I/O TRAP function codes
  (`Halt`/`Fopen`/`Fclose`/`Fread`/`Fgets`/
  `Fgetws`/`Fwrite`/`Fputs`/`Fputws`/`Fseek`/
  `Ftell`), 5 file-open modes (`TextRead`/
  `TextWrite`/`BinaryRead`/`BinaryWrite`/
  `BinaryReadWrite`), 3 standard streams
  (`StdIn`/`StdOut`/`StdErr`).

**Style routing (11 mappings across 18
defined SCE slots):** COMMENT → `Comment`
(italic — MMIXAL comments are anything after
operands with no comment-char prefix); LABEL
→ `Keyword2` (accent for column-0 label
declarations); OPCODE_VALID → `Keyword` (bold
— CPU instruction mnemonics as visual anchor,
same discipline as Erlang KEYWORD / Forth
CONTROL+KEYWORD); NUMBER + HEX → `Number`
(decimal + `#`-prefixed hex); CHAR + STRING →
`String` (both `'...'` char and `"..."`
string literals); REGISTER + SYMBOL →
`Keyword2` (register aliases like `rA` and
predefined symbols like `Fputs` share the
accent slot with LABEL — all three name
storage or named values); OPERATOR →
`Operator` (`+-|^*/%<>&~$,()[]` from
`isMMIXALOperator` at `:39-49`); INCLUDE →
`Preprocessor` (`@include` directive). Seven
slots unmapped per framework convention:
LEADWS (0), OPCODE (3), OPCODE_PRE (4),
OPCODE_UNKNOWN (6), OPCODE_POST (7), OPERANDS
(8), REF (10) — transient states and
STYLE_DEFAULT fallbacks. OPCODE_UNKNOWN in
particular stays unmapped so unrecognized
opcode-position tokens (likely user-defined
macros) paint at STYLE_DEFAULT rather than
being mis-styled.

**Case-SENSITIVE by design.** MMIXAL
convention writes opcodes in uppercase
(`ADD`/`TRAP`/`LDO`), special registers as
lowercase `r` + uppercase (`rA`/`rBB`/`rZZ`),
predefined symbols in mixed case (`Fputs`/
`StdOut`/`ROUND_NEAR`). The lexer's
`GetCurrent` at `:104, :123` (NOT
`GetCurrentLowered`) probes wordlists
byte-exact — exact spelling required.

**Line-based lexer.** Unlike most Scintilla
lexers, `LexMMIXAL.cxx:64-70` starts every
line in `SCE_MMIXAL_LEADWS` or (for
`@i`-prefix lines) `SCE_MMIXAL_INCLUDE`. The
first non-whitespace character in a LEADWS
line dispatches at `:72-83`: column-0 word
char → LABEL, column-0 non-word char →
COMMENT (no `%` required — anything not
label-shaped starts a comment), post-
whitespace word char → OPCODE_PRE → OPCODE.
After the opcode, `:154-172` dispatches
operands: digit → NUMBER, word or `@` → REF,
`"` → STRING, `'` → CHAR, `$` → REGISTER
(numeric $-register), `#` → HEX, symbolic
operator → OPERATOR, whitespace → COMMENT.

**REF settle with base-prefix stripping.**
At `:101-115`, when the REF collect state
ends, `sc.GetCurrent(s0, ...)` captures the
identifier byte-exact; if it begins with `:`,
`:106-108` strips the leading colon before
probing. This handles MMIXAL's `:GlobalName`
base-prefix syntax at the lexer level, so
wordlist entries here are NOT `:`-prefixed.
Probes class 1 (`special_register`) → 2
(`predef_symbols`) first-match-wins.

**No fold.** `LexMMIXAL.cxx:185` registers
`0` as the fold function.

**Cross-class disjointness — 3 pairs
enforced.** REF settle at `:101-115` probes
class 1 before class 2 first-match-wins; a
duplicate in class 1 would silently mask its
class-2 sibling. Class 0 is probed in a
distinct state (OPCODE), so its disjointness
with 1 and 2 is structural rather than
first-match-wins-critical, but Invariant #6
pins all three pairs anyway.

Structural test coverage: 19 invariants —
deep-value identity pin, 11-mapping style
count (18 defined slots minus 7 unmapped),
three-class canonical descriptor order, all
classes non-empty, case-sensitive byte-exact
alphabet enforcement (ASCII alnum + `_` +
`:`) applied uniformly across every class,
plus a hard no-leading-`:` pin per token
(load-bearing — LexMMIXAL.cxx:106-108 strips
one leading `:` before the InList probe, so
`:`-prefixed wordlist entries are dead code),
**pairwise cross-class disjointness across
all 3 class-pair combinations**, style-
routing pins for all 11 mapped SCE constants,
7 unmapped slots confirmed absent (LEADWS +
OPCODE + OPCODE_PRE + OPCODE_UNKNOWN +
OPCODE_POST + OPERANDS + REF), italic set ==
1 (COMMENT only), bold set == 1
(OPCODE_VALID only — single-class bold
matches CSound's OPCODE precedent among
three-class siblings; wider-class lexers pair
in a second class), cross-language non-reuse
against Forth / Erlang / ESCRIPT / CSound,
`L_MMIXAL` `LangEntry`'s `lexer:
Some("mmixal")` + `mms` extension presence,
canonical opcode anchors covering **every
`concat!()` line-literal** in
`MMIXAL_OPCODES` (35 anchors across all
opcode families) so a silent deletion of any
single line-literal is caught, canonical
special-register anchors (`rA`/`rJ`/`rZ` +
`rBB`/`rZZ` — both primary and shadow
families pinned), canonical predefined-symbol
anchors (`Inf`/`ROUND_NEAR`/`Data_Segment`/
`Fputs`/`BinaryRead`/`StdOut` — one per
sub-family), **digit-prefix opcodes present**
(`2ADDU`/`2ADDUI`/`4ADDU`/`4ADDUI`/`8ADDU`/
`8ADDUI`/`16ADDU`/`16ADDUI` all pinned),
**no `-B` fwd/back-suffix branch mnemonics
at source level** across three families:
plain branches (`BNB`/`BZB`/`BPB`/`BODB`/
`BNNB`/`BNZB`/`BNPB`/`BEVB`), predict
branches (`PBNB`/`PBZB`/`PBPB`/`PBODB`/
`PBNNB`/`PBNZB`/`PBNPB`/`PBEVB`), and jump
(`JMPB`) — 17 affirmative-absence tokens
total. No-duplicate defence-in-depth check
across all three wordlists. **Highest
defined SCE_MMIXAL_* pin** — `SCE_MMIXAL_INCLUDE`
(17) is the top slot, and no `MMIXAL_STYLES`
entry may reference a higher index; catches
a future Lexilla submodule bump that appends
a slot (would otherwise leave the style-count
and unmapped-slot pins silently passing while
the new slot renders at `STYLE_DEFAULT`).
**`OPCODE_UNKNOWN` (6) is deliberately
excluded from the deferred-`StyleSlot::Error`
migration list** — user-defined macros
legitimately hit that state (unlike
STRINGEOL / *_ERROR which are unambiguous
parse failures).

**Nim (2026-07-07):** wires `SCLEX_NIM`
(= 126) for Nim `.nim` source — the
statically-typed compiled systems programming
language with Python-like indentation-based
syntax. `L_NIM` (id 76) is the sole language
row using this lexer.

**Single-class wordlist (66 tokens).** Nim's
grammar reserves exactly 66 keywords per
manual §3.2, WebFetch-verified via two
independent retrievals and adversarially
verified per-token in the research workflow.
`NIM_KEYWORDS` covers seven functional
groups:
- **Word operators (15)**: `and`/`or`/`not`/
  `xor`/`shl`/`shr`/`div`/`mod`/`in`/`notin`/
  `is`/`isnot`/`of`/`as`/`from`. Routed
  through the identifier collect path
  (`LexNim.cxx:689-690` → `:446-462` wordlist
  probe → `SCE_NIM_WORD`), NOT the symbolic
  operator set at `:713`.
- **Control flow (18)**: `if`/`elif`/`else`/
  `when`/`case`/`of`/`for`/`while`/`break`/
  `continue`/`return`/`yield`/`discard`/
  `raise`/`try`/`except`/`finally`/`defer`.
- **Declaration / routine (12)**: `proc`/
  `func`/`method`/`iterator`/`converter`/
  `template`/`macro`/`type`/`const`/`let`/
  `var`/`using`. The seven definition
  keywords (`proc`/`func`/`method`/`iterator`/
  `converter`/`template`/`macro`) are
  load-bearing — `IsFuncName` at
  `LexNim.cxx:85-103` hardcodes exactly this
  set to trigger the `funcNameExists` flag
  that auto-styles the following identifier
  as `SCE_NIM_FUNCNAME`. Invariant 14 pins
  their presence in the wordlist.
- **Module system (4)**: `import`/`from`/
  `export`/`include`.
- **Type / structure (7)**: `object`/`tuple`/
  `enum`/`ref`/`ptr`/`distinct`/`concept`.
- **Meta / low-level (8)**: `static`/`asm`/
  `bind`/`mixin`/`addr`/`cast`/`out`/`do`.
- **Blocks + reserved-for-future (3)**:
  `block`/`end`/`interface`. Manual
  footnote: `end` and `interface` are
  reserved but currently unused by the
  compiler.
- **Special value (1)**: `nil`. Manual lists
  it inside the reserved-keyword table, NOT
  as a predefined identifier (contrast with
  `true`/`false` which are `system.bool`
  values).

(Group counts sum to more than 66 because
`of` and `from` each belong to two functional
groupings — single reserved tokens each,
counted once in the wordlist.)

**Style routing (13 mappings across 17
defined SCE slots):** All four comment
sub-styles (COMMENT + COMMENTDOC +
COMMENTLINE + COMMENTLINEDOC) → `Comment`
(italic — universal Code++ comment
convention); WORD → `Keyword` (bold —
reserved-keyword hits as visual anchor,
matching Python's WORD → bold as the closest
single-class sibling); NUMBER → `Number`;
STRING + CHARACTER + TRIPLE + TRIPLEDOUBLE →
`String` (four string variants collapse to
the shared String slot); BACKTICKS +
FUNCNAME → `Keyword2` (accent for
identifier-definition markers — backtick-
quoted spans name operators or use reserved
words as identifiers; FUNCNAME is auto-
styled after `proc`/`func`/`macro`/`method`/
`template`/`iterator`/`converter` per the
paint loop); OPERATOR → `Operator`. Four
slots unmapped: DEFAULT (0), STRINGEOL (13)
+ NUMERROR (14) both belong to the
deferred-`StyleSlot::Error` migration list —
sweep into `StyleSlot::Error` when that
migration lands (unlike MMIXAL's
`OPCODE_UNKNOWN`, Nim's STRINGEOL / NUMERROR
are unambiguous parse failures), IDENTIFIER
(16) is the transient identifier-collect
state (per framework convention, unmatched
identifiers paint at `STYLE_DEFAULT`).

**Case-SENSITIVE.** `LexNim.cxx:447` uses
`sc.GetCurrent` (NOT `GetCurrentLowered`)
for the wordlist probe. Nim's language-level
identifier comparison is partial-case-
insensitive with underscore collapse
(`fooBar` == `foo_bar` == `FOOBAR`), but the
lexer's wordlist probe is a plain byte-exact
`WordList::InList` lookup. Nim source
overwhelmingly writes keywords lowercase per
the official style guide, so all 66
wordlist tokens are lowercase.

**Auto-styled FUNCNAME after definition
keywords.** At `LexNim.cxx:446-465` and
`:681-687`, when a keyword identifier
matches `IsFuncName(s)` (one of the seven
def keywords) the paint loop sets
`funcNameExists = true`; the NEXT identifier
or backtick span gets emitted as
`SCE_NIM_FUNCNAME` instead of `IDENTIFIER`/
`BACKTICKS`. Entirely paint-loop-driven — no
wordlist support needed — but the seven def
keywords must be in the wordlist for the
`InList` probe at `:452` to return true and
trigger the flag flip.

**Rich string family — 6 entry paths.**
LexNim's paint loop at `:625-679` covers:
bare `"..."` → STRING; `"""..."""` triple-
double → TRIPLEDOUBLE (with special handling
for up-to-5 opening quotes); `'x'` char →
CHARACTER; `'''...'''` triple-single →
TRIPLE; `r"..."` / `R"..."` raw string →
STRING with `isStylingRawString` flag;
generalized raw `xyz"..."` → configurable via
`lexer.nim.raw.strings.highlight.ident`.

**Comment family — 4 sub-styles.** At
`:693-711`: `##[` → COMMENTDOC (nestable
block doc); `#[` → COMMENT (nestable block);
`##` → COMMENTLINEDOC (line doc); `#` →
COMMENTLINE. Block comments are nestable per
Nim spec — the lexer tracks `commentNestLevel`
in `styler.SetLineState`.

**Fold** uses indentation levels via
`IndentAmount` at `:164-168` (Python-style
indent-based folding), NOT brace or
keyword-based folding.

Structural test coverage: 16 invariants —
deep-value identity pin, 13-mapping style
count (17 defined slots minus 4 unmapped),
single wordlist class 0 → `NIM_KEYWORDS`
mapping, **exact 66-token count** matching
Nim manual §3.2, all-lowercase alphabet
enforcement, style-routing pins for all 13
mapped SCE constants, 4 unmapped slots
confirmed absent (`DEFAULT` + `STRINGEOL` +
`NUMERROR` + `IDENTIFIER`), italic set == 4
(all four comment sub-styles), bold set == 1
(WORD only), cross-language non-reuse
against Forth / Rust / MMIXAL / CSound,
`L_NIM` `LangEntry`'s `lexer: Some("nim")` +
`nim` extension presence, canonical keyword
anchors covering all seven functional groups
(50+ anchor tokens), **affirmative absence
pins** for 16 tokens commonly assumed to be
Nim keywords but aren't (`true`/`false`/
`echo`/`result`/`int`/`string`/`bool`/
`float`/`char`/`seq`/`array`/`generic`/
`atomic`/`raises`/`gcsafe`/`inline` — load-
bearing because a wordlist entry for any of
these would silently mis-style ordinary
identifiers as `SCE_NIM_WORD`), **all seven
definition keywords** (`proc`/`func`/`macro`/
`method`/`template`/`iterator`/`converter`)
pinned as present because `IsFuncName` at
`LexNim.cxx:85-103` requires each of them
individually — a missing keyword breaks the
`funcNameExists` flag flip for that def
style, cascading into missing `FUNCNAME`
styling for the following identifier —
**highest defined `SCE_NIM_*` pin**
(`SCE_NIM_IDENTIFIER` (16) as top slot;
catches future Lexilla submodule bumps), and
no-duplicate defence-in-depth check.

**NNCrontab (2026-07-07):** wires
`SCLEX_NNCRONTAB` (= 26) for nnCron's
extended crontab format — a Windows scheduler
/ event monitor by Nick Nemtsev
(<https://www.nncron.ru/>) that uses Forth as
its embedded scripting language on top of
cron-style time specifications (extension
`.tab`). `L_NNCRONTAB` (id 77) is the sole
language row using this lexer. Three-class
descriptor same shape as CSound / MMIXAL /
ESCRIPT.

**Three-class wordlist (44 + 174 + 38 = 256
tokens):**
- **Class 0 (`SCE_NNCRONTAB_SECTION`, bold
  Keyword)** — `NNCRONTAB_SECTIONS` carries
  44 tokens: 11 nnCron section markers
  (`Task`/`Time`/`Rule`/`When`/`Action`/
  `Days`/`Hours`/`Minutes`/`Months`/
  `WeekDays`/`Years`) that label a task
  definition's structural fields, plus 33
  Forth core control/arithmetic/logic/
  defining words (`IF`/`THEN`/`ELSE`/`BEGIN`/
  `UNTIL`/`WHILE`/`REPEAT`/`AGAIN`/`DO`/
  `LOOP`/`LEAVE`/`CASE`/`OF`/`ENDOF`/
  `ENDCASE`/`AND`/`OR`/`NOT`/`TRUE`/`FALSE`/
  `ON`/`OFF`/`SET`/`I`/`CONSTANT`/
  `VARIABLE`/`CREATE`/`VALUE`/`ALLOT`/`PAD`/
  `EVALUATE`/`EVAL-SUBST`/`COMPARE`) — nnCron
  embeds Forth as its scripting language
  (`PAD` is Forth's scratch-buffer word,
  bundled into class 0 with the other Forth
  core words per SciTE's canonical
  descriptor).
- **Class 1 (`SCE_NNCRONTAB_KEYWORD`, bold
  Keyword)** — `NNCRONTAB_KEYWORDS` carries
  174 tokens across 14 functional groups:
  file/directory operations, time/date
  built-in variable readers (`Day@`/`Hour@`/
  etc. with `@`-suffix), watch triggers,
  RAS/dialup, logon credentials, registry,
  dialogs, mouse+keyboard, sound/power,
  process control, windows manipulation,
  POP3/clipboard/logging, regex, and script
  embedding markers (`<JScript>`/`<VBScript>`
  etc.). Collapses to the same bold slot as
  SECTION — both are "language vocabulary"
  semantically, same discipline as Forth
  CONTROL + KEYWORD → bold.
- **Class 2 (`SCE_NNCRONTAB_MODIFIER`,
  Keyword2 accent)** — `NNCRONTAB_MODIFIERS`
  carries 38 task-execution attributes: 6
  priority classes (`AboveNormalPriority`,
  `HighPriority`, `RealtimePriority`, etc.),
  5 window-state hints (`ShowMaximized` etc.),
  3 startup positioning, 4 once-a-N
  scheduling qualifiers (`OnceADay`/`OnceAHour`
  etc.), run-once/service/no-flags, auth,
  recursion/depth flags, 6
  `WATCH-CHANGE-*` file-watcher change flags,
  and watch-subtree. Cross-referenced against
  nnCron's task-options and watch
  documentation.

**Style routing (9 mappings across 11
defined SCE slots):** COMMENT → `Comment`
(italic — both `#`-to-EOL and Forth-style
`\ `-to-EOL); TASK → `Preprocessor` (the
`#(...)` and `)#` task-delimiter markers
structurally frame each task definition —
same "meta-file-structure" slot as MMIXAL's
`@include` directive); SECTION + KEYWORD →
`Keyword` (bold — two "language vocabulary"
classes collapse, same discipline as Forth
CONTROL + KEYWORD → bold); MODIFIER →
`Keyword2` (accent for task-execution
attributes distinguishing them from action
verbs); ASTERISK → `Operator` (the `*` cron
wildcard is semantically an operator on the
time-field grammar); NUMBER → `Number`;
STRING → `String`; ENVIRONMENT → `Keyword2`
(`%VAR%` and `<%VAR%>` environment expansions
are named-value references, accent matches
MMIXAL SYMBOL, Nim FUNCNAME). Two slots
unmapped: DEFAULT (0) and IDENTIFIER (10) —
transient states / STYLE_DEFAULT fallback
per framework convention (unmatched
identifiers paint plainly).

**Source:** `nncrontab.properties` from
SciTE's language-config catalog
(<https://raw.githubusercontent.com/SciTe-Community/color-highlighter/master/nncrontab.properties>),
cross-referenced against nnCron's own
documentation for section markers, task
options, and watch directives.

**Case-SENSITIVE.** `LexCrontab.cxx:181-196`
uses `WordList::InList` with no lowering.
Every entry is in the canonical spelling
nnCron source uses.

**Hand-rolled state machine, no `StyleContext`.**
Unlike most Lexilla lexers, `LexCrontab.cxx`
uses a raw `switch(state)` loop with manual
`styler.ColourTo` calls (`:63-215`) rather
than the modern `StyleContext` API. This is
a legacy Lexilla lexer with vintage-1998
idioms — even a hand-allocated `char*
buffer = new char[length+1]` at `:40` and a
matching `delete[]` at `:217`. Paint-loop-
internal, no host-visible impact.

**Wide identifier alphabet** at `:175-177`:
alnum + `_` + `-` + `/` + `$` + `.` + `<` +
`>` + `@`. Supports directive-argument
identifiers with embedded delimiters:
`FILE-COPY` (hyphen), `Day@`/`Hour@`
(at-suffix reader convention), `<JScript>`
(angle-bracketed script embedding markers).
Invariant 5 pins this alphabet across every
wordlist token — any byte outside the set
would produce a dead entry the paint loop's
identifier-collect state could never emit.

**First-match-wins cascade** at `:181-196`
probes classes 0 → 1 → 2 in exact order.
Cross-class disjointness is required —
Invariant 6 enforces it pairwise across all
three class-pair combinations.

**String / environment interleaving.** Inside
STRING at `:141-146`, a `%` transitions to
ENVIRONMENT with `insideString = true`; from
ENVIRONMENT at `:159-163`, a `%` with
`insideString` true transitions back to
STRING. Supports `"...text %ENV_VAR% more
text..."` where the environment expansion is
styled distinctly inside a string.

**No fold.** `LexCrontab.cxx:227` registers
`0` as the fold function.

Structural test coverage: 17 invariants —
deep-value identity pin, 9-mapping style
count (11 defined slots minus 2 unmapped),
three-class canonical descriptor order, all
classes non-empty, byte-exact `IsAWordChar`
alphabet enforcement (alnum + `_` + `-` +
`/` + `$` + `.` + `<` + `>` + `@`) applied
uniformly across every class, **pairwise
cross-class disjointness across all 3
class-pair combinations** (load-bearing for
the first-match-wins cascade at
`:181-196`), style-routing pins for all 9
mapped SCE constants, 2 unmapped slots
confirmed absent (DEFAULT + IDENTIFIER),
italic set == 1 (COMMENT only), bold set ==
2 (SECTION + KEYWORD), cross-language
non-reuse (Forth / Erlang / MMIXAL /
CSound), `L_NNCRONTAB` `LangEntry`'s
`lexer: Some("nncrontab")` + `tab`
extension presence, canonical section
anchors (`Task`/`IF`/`TRUE`/`ALLOT` — one
per `concat!()` line), canonical keyword
anchors (20 anchors covering all 14
functional groups — `RUN`/`FILE-COPY`/
`WIN-ACTIVATE`/`MOUSE-LBCLK`/`Day@`/
`WatchFile`/`DIAL`/`Password`/`REG-DWORD`/
`MSG`/`BEEP`/`POP3-CHECK`/`RE-MATCH`/
`<JScript>`/`SEND-KEYS`/`GetTickCount`/
`EXIST`/`CONSOLE`/`PLAY-SOUND`/`CHAR`),
canonical modifier anchors (9 anchors — one
per sub-family), highest-defined
`SCE_NNCRONTAB_*` pin
(`SCE_NNCRONTAB_IDENTIFIER=10` as top slot;
catches future Lexilla submodule bumps),
and no-duplicate defence-in-depth check.

**OScript (2026-07-07):** wires `SCLEX_OSCRIPT`
(= 106) for OScript `.osx` source — the
object-oriented programming language for
OpenText Livelink (now OpenText Content
Server). `L_OSCRIPT` (id 78) is the sole
language row using this lexer. Six-class
descriptor — the widest of Phase 4.5 (tied
with Forth 6 and Erlang 6).

**Six-class wordlist (32 + 22 + 10 + 69 +
23 + 17 = 173 tokens, all lowercase because
`LexOScript.cxx:141` calls
`sc.GetCurrentLowered` before every wordlist
probe):**
- **Class 0 (`SCE_OSCRIPT_KEYWORD`, bold
  Keyword)** — `OSCRIPT_KEYWORDS` carries 32
  tokens: 17 control-flow words (`if`/`else`/
  `elseif`/`end`/`for`/`while`/`repeat`/
  `until`/`switch`/`case`/`default`/`break`/
  `breakif`/`continue`/`continueif`/`goto`/
  `return`), 4 loop-range qualifiers
  (`by`/`downto`/`in`/`to` used in `for i =
  1 to 10 by 2` / `for x in list`), 5
  function/declaration keywords (`function`/
  `void`/`dll`/`xcmd`/`xfcn` — the latter
  two are HyperCard-legacy external-command
  and external-function markers), and 6
  modifiers (`inbyref`/`inout`/`linked`/
  `nodebug`/`super`/`this`). `end` is
  OScript's universal block terminator — no
  `then`/`endif`/`wend` in the grammar.
- **Class 1 (`SCE_OSCRIPT_CONSTANT`,
  Keyword2 accent)** — `OSCRIPT_CONSTANTS`
  carries 22 tokens: 3 value literals
  (`true`/`false`/`undefined` — `undefined`
  returns from unbound identifier lookups),
  and 19 type-identifier constants
  (`integertype`/`stringtype`/`assoctype`/
  etc.) used in reflection comparisons like
  `x.DataType == IntegerType`. Distinct from
  class 3 (TYPES) which holds the declaration-
  side type keywords.
- **Class 2 (`SCE_OSCRIPT_OPERATOR`,
  Operator)** — `OSCRIPT_OPERATORS` carries
  10 word-form operators: 4 logical
  (`and`/`or`/`not`/`xor`) and 6 relational
  (`eq`/`ne`/`lt`/`le`/`gt`/`ge`). OScript
  accepts both symbolic (`==`/`!=`/`<`/etc.)
  and word forms for readability. `in`
  deliberately excluded — it's a `for-in`
  loop KEYWORD, not an operator.
- **Class 3 (`SCE_OSCRIPT_TYPE`, Keyword2
  accent)** — `OSCRIPT_TYPES` carries 69
  tokens across four families: 18 primitive
  value types (`integer`/`string`/`boolean`/
  `real`/`list`/`assoc`/etc.), ~31 Livelink
  CAPI/DAPI/UAPI/WAPI object types
  (`dapinode`/`dapisession`/`capilogin`/
  `wapisession`/`sqlconnection`/`socket`/
  etc.), 18 DOM Level 1/2 interfaces
  (`domdocument`/`domelement`/`domattr`/
  `domnodelist`/etc.), and 2 XML parser
  types (`saxparser`/`xslprocessor`).
- **Class 4 (`SCE_OSCRIPT_FUNCTION`,
  Keyword2 accent)** — `OSCRIPT_FUNCTIONS`
  carries 23 tokens: 6 debug/echo output
  (`echo`/`echodebug`/`echoerror`/`echoinfo`/
  `echostamp`/`echowarn`), 9 `is*` type /
  state predicates (`isdefined`/`iserror`/
  `isobject`/`isset`/`isundefined`/etc.), 6
  reflection helpers (`datatypename`/
  `getfeatures`/`length`/`nparameters`/
  `parameters`/`type`), and 2 point
  constructors (`pointh`/`pointv`).
- **Class 5 (`SCE_OSCRIPT_OBJECT`, Keyword2
  accent)** — `OSCRIPT_OBJECTS` carries 17
  Livelink singletons: 5 Livelink Content
  Server APIs (`capi`/`dapi`/`uapi`/`wapi`/
  `web`), 3 utility namespaces (`math`/
  `str`/`system`), 3 logging channels
  (`console`/`debug`/`err`), 5 file/kernel/
  parser/patch/prgctx namespaces, and 1
  script namespace. Class 5 is probed ONLY
  in the dot-suffix path at
  `LexOScript.cxx:163` — disjoint from
  classes 0-4 at the paint-loop level, so
  `script` and `file` legitimately appear
  in BOTH TYPES (class 3) and OBJECTS
  (class 5): `Script s = ...` styles as
  TYPE, `Script.Compile(...)` styles as
  OBJECT.

**Style routing (17 mappings across 19
defined SCE slots):** LINE_COMMENT +
BLOCK_COMMENT + DOC_COMMENT → `Comment`
(italic — three sub-styles collapse;
DOC_COMMENT covers the `#ifdef DOC ...
#endif` conditional-preprocessor
documentation block); PREPROCESSOR →
`Preprocessor` (`#ifdef`/`#ifndef`/
`#endif`); NUMBER → `Number`;
SINGLEQUOTE_STRING + DOUBLEQUOTE_STRING →
`String`; CONSTANT + GLOBAL + LABEL + TYPE
+ FUNCTION + OBJECT + PROPERTY + METHOD →
`Keyword2` (all named-value markers share
the accent slot — literal constants,
`$var`/`$$var` process/thread globals,
column-0 labels, declaration-side types,
library functions, Livelink singletons,
bare-access property spans, and auto-styled
method names); KEYWORD → `Keyword` (bold —
the single visual anchor, matching MMIXAL
OPCODE_VALID and CSound OPCODE precedents
for single-class bold amongst
multi-class-vocabulary lexers). OPERATOR →
`Operator` (both symbolic and word forms).
Two slots unmapped: DEFAULT (0) and
IDENTIFIER (9) transient collect state —
the wordlist-miss path relies on
IDENTIFIER's unmapping to paint plainly.
**PROPERTY (17) IS mapped** (accent) —
bare `.identifier` NOT followed by `(`
commits with state 17 via
`LexOScript.cxx:255-266`'s
`SetState(DEFAULT)` and would otherwise
render at STYLE_DEFAULT, breaking the
common `obj.propertyName := ...` idiom.

**Two-phase context-sensitive classifier.**
Unlike single-cascade lexers,
`IdentifierClassifier` at `LexOScript.cxx:132-181`
dispatches by syntactic context:
- **Parenthesis path** (`sc.Match('(')`):
  probes KEYWORD → OPERATOR → FUNCTION →
  METHOD (default when identifier followed
  by `(` misses all wordlist classes).
- **Dot-suffix path** (`sc.Match('.') &&
  objects.InList(s)`): probes OBJECT only.
- **No-paren, no-dot path**: probes KEYWORD
  → CONSTANT → OPERATOR → TYPE → FUNCTION
  first-match-wins.

**Case-INSENSITIVE.** OScript source may
write reserved words in any case (`If`, `IF`,
`if` all valid). The lexer lowercases before
probing.

**Auto-styled LABEL / PROPERTY / METHOD**
without wordlist support: column-0
`identifier:` becomes LABEL at `:241-243`;
`.identifier` enters PROPERTY at `:345-355`
and upgrades to METHOD at `:262-263` if
followed by `(`.

**GLOBAL** (`SCE_OSCRIPT_GLOBAL`): `$xxx` /
`$$xxx` process / thread-global variables at
`:336-339`.

**Source:** SciTE's `oscript.properties`
catalog, cross-referenced against Ferdinand
Prantl's Notepad++ OScript UDL (same author
as `LexOScript.cxx` per file header at
`:1-9`).

Structural test coverage: 22 invariants —
deep-value identity pin, 17-mapping style
count (19 defined slots minus 2 unmapped —
DEFAULT + IDENTIFIER; PROPERTY IS mapped),
six-class canonical descriptor order, all
classes non-empty, all-lowercase
alphanumeric+underscore alphabet enforcement
across every class, **cross-class
disjointness across classes 0-4** (the
first-match-wins paren + no-paren cascade at
`:139-176`; class 5 OBJECT is EXEMPT because
it's probed only in the dot-suffix path at
`:163` and legitimately overlaps class 3
TYPES on `script`/`file`), style-routing
pins for all 16 mapped SCE constants, 3
unmapped slots confirmed absent
(DEFAULT/IDENTIFIER/PROPERTY), italic set
== 3 (all three comment sub-styles), bold
set == 1 (KEYWORD only), cross-language
non-reuse (Forth / Nim / MMIXAL / CSound),
`L_OSCRIPT` `LangEntry`'s `lexer:
Some("oscript")` + `osx` extension
presence, 4 canonical keyword anchors (one
per functional group), 5 canonical constant
anchors, 7 canonical operator anchors
covering logical + relational, 4 canonical
type anchors (one per family), 4 canonical
function anchors (one per family), 4
canonical object anchors (one per family),
**highest-defined `SCE_OSCRIPT_*` pin**
(`SCE_OSCRIPT_METHOD` (18) as top slot),
**affirmative absence pins** for tokens
commonly assumed to be OScript keywords but
aren't (`then`/`endif`/`wend` in KEYWORDS,
`mod`/`div`/`in` in OPERATORS),
no-duplicate defence-in-depth check, and
**class-5 legitimate-overlap pin** (`script`
and `file` MUST appear in both TYPES and
OBJECTS — the paint-loop's context-scoped
probe makes this legal; removing them from
either would break OScript idioms).

**REBOL (2026-07-07):** wires `SCLEX_REBOL`
(= 71) for REBOL `.reb` / `.rebol` source —
Carl Sassenrath's homoiconic message-passing
dialect language. `L_REBOL` (id 79) is the
sole language row using this lexer.
**Eight-class descriptor** — widest of
Phase 4.5, exceeding Forth 6, Erlang 6, and
OScript 6. Even though upstream registration
at `LexRebol.cxx:320-323` declares only
`{"Keywords", 0}` (single-class), the paint
loop `ColouriseRebolDoc` accesses all 8
wordlist slots via `keywordlists[0..7]`
at `:74-81` and emits the corresponding
`SCE_REBOL_WORD..WORD8` states — SciTE /
Notepad++ populate the additional slots
via `SCI_SETKEYWORDS(N, ...)` even though
only slot 0 is exposed by descriptor.

**Five populated classes (47 + 59 + 71 + 79
+ 50 = 306 tokens):**
- **Class 0 (`SCE_REBOL_WORD`, bold
  Keyword)** — `REBOL_WORD` carries 47
  primary keywords: 24 control-flow
  (`if`/`either`/`unless`/`while`/`until`/
  `loop`/`repeat`/`for`/`forall`/`foreach`/
  `forever`/`forskip`/`break`/`continue`/
  `return`/`exit`/`catch`/`throw`/`halt`/
  `try`/`attempt`/`switch`/`case`), 15
  definition / evaluation (`do`/`does`/
  `func`/`function`/`has`/`use`/`make`/
  `context`/`construct`/`bind`/`in`/
  `reduce`/`compose`/`get`/`set`), 2
  special (`quit`/`comment` — `comment`
  gets Keyword styling here for the word
  that introduces `comment {...}` block
  comments; NOTE the block-comment flag
  flip at `LexRebol.cxx:161`
  (`blockComment = strcmp(s, "comment")
  == 0;`) runs UNCONDITIONALLY before any
  wordlist probe, so wordlist membership
  is purely cosmetic — removing `comment`
  from WORD would not break block-comment
  detection, only its paint style),
  and 6 logical / short-circuit
  (`not`/`and`/`or`/`xor`/`any`/`all`).
- **Class 1 (`SCE_REBOL_WORD2`, Keyword2
  accent)** — `REBOL_WORD2` carries 59
  datatypes, all with trailing `!` per
  REBOL convention: 27 value types
  (`integer!`/`string!`/`block!`/`char!`/
  `binary!`/`bitset!`/`file!`/`date!`/
  etc.), 7 function-like (`action!`/
  `function!`/`native!`/`op!`/`routine!`/
  `command!`/`closure!`), 6 word / path
  variants (`get-word!`/`lit-word!`/
  `set-word!`/`refinement!`/`lit-path!`/
  `set-path!`), 11 typesets / collections
  (`any-block!`/`any-function!`/
  `any-string!`/`any-type!`/`any-word!`/
  `series!`/`number!`/`typeset!`/
  `datatype!`/`list!`/`library!`), and 8
  REBOL 3 additions (`vector!`/`map!`/
  `percent!`/`gob!`/`handle!`/`port!`/
  `object!`/`struct!`).
- **Class 2 (`SCE_REBOL_WORD3`, Keyword2
  accent)** — `REBOL_WORD3` carries 71
  math + conversion natives: 31 arithmetic
  / trig (`absolute`/`sine`/`cosine`/`round`/
  `random`/`power`/`negate`/`shift`/`etc.`),
  35 `to-*` conversions (`to`/`to-integer`/
  `to-string`/`to-binary`/`to-file`/etc.),
  and 5 encoding helpers (`as-pair`/
  `charset`/`debase`/`dehex`/`enbase`).
- **Class 3 (`SCE_REBOL_WORD4`, Keyword2
  accent)** — `REBOL_WORD4` carries 79 I/O
  and system natives across 10 groups:
  console I/O (`print`/`prin`/`input`/
  `ask`/`probe`), file / network I/O
  (`open`/`close`/`read`/`write`/`save`/
  `load`/`delete`/`wait`/`send`/etc.),
  requests / dialogs (`request-file`/
  `request-color`/etc.), query /
  introspection (`query`/`exists?`/
  `script?`/`modified?`/`info?`/etc.),
  file-system helpers (`make-dir`/
  `change-dir`/`clean-path`/etc.), view /
  display (`layout`/`view`/`focus`/`show`/
  `hide`/etc.), security / protection
  (`secure`/`protect`/`recycle`), program
  control (`launch`/`browse`/`echo`/
  `trace`/etc.), confirmation / parsing
  (`confirm`/`import-email`/`decode-cgi`/
  `parse-xml`/`build-tag`), compression /
  encoding (`compress`/`decompress`/
  `load-image`), and system / event
  (`now`/`do-events`/`resend`).
- **Class 4 (`SCE_REBOL_WORD5`, Keyword2
  accent)** — `REBOL_WORD5` carries 50
  series / block operations across 8
  groups: mutating (`append`/`insert`/
  `remove`/`change`/`clear`/`poke`),
  non-mutating access (`copy`/`find`/
  `at`/`back`/`next`/`head`/`tail`/`pick`/
  `select`/`extract`/`skip`/`remove-each`),
  reordering (`reverse`/`sort`/`unique`),
  set-like operations (`intersect`/
  `union`/`difference`/`exclude`),
  positional accessors (`first`/`second`/
  `third`/`fourth`/`fifth`/`last`), query
  (`index?`/`length?`/`offset?`/`size?`/
  `series?`), string helpers (`repend`/
  `replace`/`join`/`rejoin`/`parse`/
  `trim`/`remold`/`reform`), casing
  (`lowercase`/`uppercase`), and misc
  (`alter`/`detab`/`entab`/`free`).

**Classes 5-7 left EMPTY** per SciTE
convention. LexRebol's paint-loop probes
them via `InList` which returns false for
every identifier — no runtime cost, and
the theme maps WORD6/WORD7/WORD8 to
Keyword2 defensively so a future
population wires visible styling
automatically.

**Reverse-first-match-wins cascade** at
`LexRebol.cxx:162-178` — probes classes
**7 → 6 → 5 → 4 → 3 → 2 → 1 → 0** in
REVERSE order. Higher-numbered classes
SHADOW lower-numbered ones on collision.
Invariant 6 enforces cross-class
disjointness across all 10 pairs of
populated classes 0-4 — a duplicate in a
HIGHER-numbered class silently masks its
LOWER-numbered sibling, so this discipline
is load-bearing.

**Style routing (27 mappings across 29
defined SCE slots):** COMMENTLINE +
COMMENTBLOCK + PREFACE → `Comment`
(italic — three "documentation prose"
states, where PREFACE is preamble text
before the `REBOL [...]` header block);
OPERATOR → `Operator` (symbolic operators
from `IsAnOperator` at `:46-63`);
CHARACTER + QUOTEDSTRING + BRACEDSTRING +
FILE + EMAIL + URL + ISSUE + TAG →
`String` (eight prefix/suffix-delimited
value literals — REBOL's first-class
`file!`/`email!`/`url!`/`issue!`/`tag!`
types all behave semantically like
strings with different delimiters);
NUMBER + PAIR + TUPLE + BINARY + MONEY +
DATE + TIME → `Number` (seven numeric
literal forms — REBOL's first-class
syntactic value types carrying numeric
semantics); WORD → `Keyword` (bold visual
anchor); WORD2/WORD3/WORD4/WORD5/WORD6/
WORD7/WORD8 → `Keyword2` (accent — seven
tiers of vocabulary accent). Two slots
unmapped: DEFAULT (0), IDENTIFIER (20)
transient collect state.

**Case-INSENSITIVE** via `sc.GetCurrentLowered`
at `:160` — wordlist tokens must be
lowercase.

**Very wide identifier alphabet.**
`IsAWordChar` at `:37-39` accepts alnum +
`? ! . ' + - * & | = _ ~` — REBOL word
names include `empty?`, `found?`,
`type-of`, and even symbolic names like
`+`/`-`/`?`/`!`. Invariant 5 enforces
this wide alphabet across every wordlist
token.

**Homoiconic value literals.** REBOL treats
many syntactic forms as first-class
values, each with its own SCE state and
paint-loop mechanic:
`identifier:` (colon suffix, no space) →
URL; `identifier@` → EMAIL; `identifier$`
→ MONEY (retroactively re-classified at
LexRebol.cxx:145-153); NUMBER post-settles
to PAIR on `x` / TIME on `:` / DATE on
`-`/`/` / TUPLE on multiple `.` at
`:185-197`; `{...}` strings nest balanced
braces at `:206-213`; `#{...}` /
`2#{...}` / `NN#{...}` binary literals at
`:65-69`.

**Preface state.** `SCE_REBOL_PREFACE` (3)
covers preamble text BEFORE the first
`REBOL [...]` header block — REBOL
convention treats everything before the
header as documentation prose, not code.
Entered at `:100` (initial state).

**Fold** at `:275+` uses brace / bracket /
paren nesting levels plus block-comment
style transitions.

**Source:** SciTE community `rebol.properties`
(https://raw.githubusercontent.com/SciTe-Community/color-highlighter/master/rebol.properties)
re-partitioned across LexRebol's 5
populated slots to match the semantic
buckets emitted by the paint loop. Upstream
ships 3 keyword slots (general vocab,
`?`-predicates, `!`-datatypes); Code++
splits them further to align with the
lexer's WORD/WORD2/WORD3/WORD4/WORD5
routing.

Structural test coverage: 22 invariants —
deep-value identity pin, 27-mapping style
count (29 defined slots minus 2
unmapped), five populated classes in
canonical descriptor order (classes 5-7
left empty per SciTE convention), all
five populated classes non-empty,
**very-wide alphabet enforcement**
(lowercase alnum + `? ! . ' + - * & | = _
~`) across every class, **pairwise
cross-class disjointness across all 10
pairs of populated classes 0-4**
(load-bearing for the reverse-first-
match-wins cascade), style-routing pins
for all 27 mapped SCE constants
including all 8 WORD tiers, 2 unmapped
slots confirmed absent, italic set == 3
(COMMENTLINE + COMMENTBLOCK + PREFACE),
bold set == 1 (WORD only),
cross-language non-reuse (Forth /
OScript / MMIXAL / `CSound`),
`L_REBOL` `LangEntry`'s `lexer:
Some("rebol")` + both `reb` and `rebol`
extensions, canonical WORD anchors
(`if`/`func`/`comment`/`any`), canonical
WORD2 datatypes (all `!`-suffixed),
canonical WORD3/WORD4/WORD5 anchors per
functional group, **`comment` cosmetic
styling pin** (retained in WORD for
Keyword-bold styling of REBOL's block-
comment-introducing word; note the
`blockComment` flag flip at
`LexRebol.cxx:161` is UNCONDITIONAL and
independent of wordlist membership),
**all WORD2 tokens end in
`!`** (REBOL datatype convention —
`!`-less entries are mis-classifications),
highest-defined `SCE_REBOL_*` pin
(`SCE_REBOL_WORD8` (28) as top slot),
**affirmative absence pins** (`abs`/
`sqrt`/`symbol!` — non-canonical
spellings; REBOL's canonical forms are
`absolute` / `square-root` / `word!`),
and no-duplicate defence-in-depth check.

**Registry (2026-07-08):** wires
`SCLEX_REGISTRY` (= 115, per
`SciLexer.h:131`) for Windows Registry
Editor export files (extension `.reg`).
The strongest **zero-wordlist** lexer in
Phase 4.5: `RegistryWordListDesc[]` at
`LexRegistry.cxx:38-40` is a bare
`{ 0 }` terminator, and
`LexerRegistry::WordListSet` at
`:191-193` unconditionally returns -1,
REJECTING any keyword install.
Classification is purely state-machine
driven — the LexRegistry `Lex` method
at `:213-355` runs a `StyleContext`
state machine with five lookahead
helpers (`AtValueName` / `AtKeyPathEnd`
/ `AtValueType` / `AtGUID` /
`IsNextNonWhitespace`) that
retroactively classify tokens based on
line shape, not identifier lookup. Same
`—` glyph in the Keywords column as
INI / Properties (both route to
`props`, another zero-wordlist lexer)
— a `—` + ✅ theme still counts as ✅
overall since there is nothing to
wire.

**12-mapping style routing.**
`REGISTRY_STYLES` maps every SCE
constant except `SCE_REG_DEFAULT`:
`COMMENT` (1, `;`-to-EOL) → Comment;
`VALUENAME` (2, `"..."` on LHS of `=`)
+ `STRING` (3, `"..."` on RHS) →
String; `HEXDIGIT` (4, comma-separated
binary or dword hex value tails) →
Number; `VALUETYPE` (5, `dword` /
`hex` / `hex(b)` / `hex(7)` type-tag
tokens) → Keyword2; `ADDEDKEY` (6,
`[HKEY_...\path]` full keypath — the
PRIMARY structural anchor) → Keyword
(bold); `DELETEDKEY` (7, `[-HKEY_...]`
deletion directive) + `PARAMETER` (11,
`%0`/`%1`/`%*` runtime-substitution
markers) → Preprocessor (both are
out-of-band directives); `ESCAPED` (8,
`\"` / `\\` escape sequences) →
Lifetime (adjacent to strings, small
distinct highlighting); `KEYPATH_GUID`
(9) + `STRING_GUID` (10, both
`{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`
GUID sub-tokens) → Macro (consistent
GUID highlighting across keypath and
string contexts); `OPERATOR` (12, the
`-,.=:\@()` character set from
`setOperators` at `:219`) → Operator.

**State-machine mechanics.** The
`highlight` flag at `:222-229, :338,
:347` gates HEXDIGIT / OPERATOR
emission on the LHS of `=`: set true
after any `=` or `@` (default-value
marker), reset per line unless the
previous line ended with `\`
(continuation). Without this, hex-
looking prose in a comment before `=`
would paint as HEXDIGIT — the flag
scopes value-tail emission correctly.
String → GUID → String nesting at
`:245-249, :300-303`: encountering `{`
inside a VALUENAME/STRING transitions
to STRING_GUID if `AtGUID` confirms a
well-formed GUID follows; the GUID's
closing `}` returns to the saved outer
state. Same nesting for ADDEDKEY /
DELETEDKEY → KEYPATH_GUID at
`:279-284, :300-303` (shared
case-label fall-through with
STRING_GUID at `:298-299`; the
outer-keypath exit at `:307-310`
fires only after the return-to-outer
has restored the state to ADDEDKEY /
DELETEDKEY).

**Fold** at `:358-413` is
header-driven: any line containing a
KEYPATH-styled span becomes a fold
header; following non-header lines
fold into the previous header's body.
Similar shape to LexOthers'
FoldPropsDoc.

Structural test coverage: 12
invariants — deep-value identity pin,
12-mapping style count (13 defined
slots minus DEFAULT), **empty-keywords
LOAD-BEARING** (WordListSet returns
-1), cross-language non-reuse across
13 sibling themes (C++ / Makefile /
Pascal / PHP / Batch / SQL / VB / CSS
/ Rust / REBOL + explicit PROPS /
LaTeX / TeX cross-pins since all four
are zero-wordlist and must remain
structurally distinct), style-routing
pins for all 12 mapped constants, 1
unmapped slot (DEFAULT) confirmed
absent with drift-pin assertion,
italic set == 1 (COMMENT only), bold
set == 1 (ADDEDKEY only — primary
structural anchor), highest-defined
`SCE_REG_*` pin (`SCE_REG_OPERATOR`
(12) as top slot), `L_REGISTRY`
`LangEntry`'s `lexer:
Some("registry")` + `.reg` extension,
**GUID-pair pin** (KEYPATH_GUID +
STRING_GUID both route to Macro for
consistent GUID highlighting), and
**directive-pair pin** (DELETEDKEY +
PARAMETER both route to Preprocessor
as out-of-band-marker semantics).

**Spice (2026-07-08):** wires
`SCLEX_SPICE` (= 78, per
`SciLexer.h:94`) for SPICE (Simulation
Program with Integrated Circuit
Emphasis) circuit netlist files
(extensions `.sp` / `.spice`).
Three-class wordlist descriptor per
`LexSpice.cxx:42-46`
(`Keywords`/`Keywords2`/`Keywords3`)
covering **simulator directive stems**
(class 0, 43 tokens), **expression
functions** (class 1, 44 tokens), and
**model / waveform / sweep / options**
(class 2, 31 tokens) — 118 total.

**Case-INsensitive** cascade
(`LexSpice.cxx:110` lowercases every
byte before wordlist probe), and
**dot-prefix stripping**
(`IsDelimiterCharacter` at `:179-201`
includes `.`, so `.tran` parses as
DELIMITER + KEYWORD with the bare stem
`tran`; wordlist entries hold the
dotless stems).

**Forward first-match-wins cascade** at
`:113-130` probes class 0 → 1 → 2 in
forward order (unlike REBOL's reverse
cascade). Cross-class disjointness is
LOAD-BEARING — invariant 6 enforces
strict non-overlap. Real ambiguity
cases resolved by canonical-class-only
placement: `if` → class 1 (function
`if(cond, a, b)` inside `{...}`
expressions dominates over `.if`
conditional-compilation directive);
`temp` → class 0 (directive `.temp`
dominates; long spelling `temper` is
in class 1); `sin` / `exp` → class 1
(math-function role dominates over
independent-source waveform
specifier); `ac` / `dc` → class 0
(analysis directives `.ac` / `.dc`
dominate over source-line inline
modifier).

**Six-mapping style routing** — 9
defined SCE_SPICE_* slots minus 3
unmapped: DEFAULT (framework
convention), IDENTIFIER (transient
collect state — visible for unmatched
bare identifiers, falls through to
STYLE_DEFAULT to match
SCE_C_IDENTIFIER convention), and
**VALUE (7) is a DEAD STATE**
(verified: zero call sites in
`LexSpice.cxx` emit the slot — no
`SetState`, `ChangeState`, or
`ForwardSetState` references; reserved
in `SciLexer.h` but never entered at
runtime). Mapped: KEYWORD → Keyword
(bold); KEYWORD2 → Keyword2; KEYWORD3
→ Preprocessor; NUMBER → Number;
DELIMITER → Operator (includes the
`.` prefix of directives); COMMENTLINE
→ Comment (italic).

**Comment convention** — two entry
paths per `:160`: line-start `*`
(traditional Berkeley SPICE) or
`*~` (SciTE / LTspice extended
in-line comment starter). Both
consume to EOL.

Structural test coverage: 19
invariants — deep-value identity pin,
6-mapping style count (9 slots minus
3 unmapped), three populated classes
in canonical descriptor order,
all-lowercase-alnum alphabet
enforcement, **pairwise cross-class
disjointness across all 3 pairs**
(load-bearing for forward
first-match-wins), style-routing pins
for all 6 mapped constants, three
unmapped slots (DEFAULT, IDENTIFIER,
VALUE) confirmed absent with
drift-pin assertions, italic set == 1
(COMMENTLINE only), bold set == 1
(KEYWORD only — primary structural
anchor), cross-language non-reuse
(C++ / REBOL / OScript / Registry),
`L_SPICE` `LangEntry`'s `lexer:
Some("spice")` + both `sp` and
`spice` extensions, canonical
KEYWORD anchors
(`tran`/`dc`/`model`/`subckt`/`end`),
canonical KEYWORD2 anchors
(`sin`/`cos`/`exp`/`if`), canonical
KEYWORD3 anchors
(`nmos`/`pnp`/`pulse`/`dec`),
**dot-stripped-stems-only pin**
(LOAD-BEARING — `.<stem>` entries
would never match), highest-defined
`SCE_SPICE_*` pin (`SCE_SPICE_COMMENTLINE`
(8) as top slot), **ambiguous-token
placement pins** (`if` → class 1,
`temp` → class 0, `temper` → class 1,
`sin`/`exp` → class 1, `ac`/`dc` →
class 0 with affirmative absence
pins from the sibling class), and
no-duplicate defence-in-depth check.

**txt2tags (2026-07-08):** wires
`SCLEX_TXT2TAGS` (= 99, per
`SciLexer.h:115`) for txt2tags
plain-text-to-many-formats converter
files (extension `.t2t`). The third
`—`-glyph lexer (joining `props` and
`registry`): the `LexerModule`
registration at `LexTxt2tags.cxx:479`
uses the 3-argument constructor
`lmTxt2tags(SCLEX_TXT2TAGS,
ColorizeTxt2tagsDoc, "txt2tags")` —
NO `wordListDesc` argument passed at
all — and the paint function's
`WordList **` parameter at `:108` is
UNNAMED, never referenced in the body.
txt2tags is a pure structural-markup
lexer classifying tokens by
line-prefix and delimiter-pair rules,
not by identifier lookup.

**22-mapping style routing** across
26 defined slots — 4 unmapped by
framework convention: DEFAULT (plain
body text), LINE_BEGIN (1, transient
line-start scan state entered on
every newline / role-exit), STRONG2
(3, **DEAD STATE** — verified zero
call sites in `LexTxt2tags.cxx` emit
the slot; reserved in `SciLexer.h`
but never entered at runtime), and
PRECHAR (12, transient
leading-whitespace scan between
LINE_BEGIN and settled role state).
Mapped: **STRONG1 → Keyword (bold)**;
**EM1 / EM2 → Keyword (italic)** for
inline `//italic//` / `__underline__`
emphasis; **HEADER1..HEADER6 → Keyword
(bold)** — all six levels of `=`/`+`
header syntax get uniform structural
prominence; **ULIST_ITEM /
OLIST_ITEM / OPTION / PREPROC /
POSTPROC → Preprocessor** —
consistent out-of-band directive
family accent; **BLOCKQUOTE / COMMENT
→ Comment (italic)** — universal
prose-out-of-band convention;
**STRIKEOUT → Keyword2** — distinct
accent for visually deprecated text;
**HRULE → Operator** — structural
divider role; **LINK / CODE / CODE2
/ CODEBK → String** — consistent
literal-value visualisation across
URLs / tables / inline code / fenced
code blocks.

**State-machine mechanics.** Line-
scope entries at `:209-345` (guarded
by `sc.state == LINE_BEGIN`) handle
headers, lists, blockquotes, code
blocks, `%!`-prefixed variants
(options / preproc / postproc /
comments). Inline entries at
`:402-470` (guarded by `sc.state ==
DEFAULT`) handle links, strong /
emphasis / strikeout, inline code,
tables. `\`-escape at `:120-123`
consumes the next byte across any
state — Markdown-style escape
convention. No dedicated ESCAPED
state.

Structural test coverage: 14
invariants — deep-value identity pin,
22-mapping style count (26 defined
slots minus 4 unmapped), **empty-
keywords LOAD-BEARING** (LexerModule
takes no wordListDesc; WordList**
parameter unnamed and never used),
cross-language non-reuse across 13
sibling themes including explicit
zero-wordlist cross-pins (PROPS /
LaTeX / TeX / Registry), style-
routing pins for all 22 mapped
constants, 4 unmapped slots confirmed
absent with drift-pin assertions,
italic set == 4 (COMMENT / BLOCKQUOTE
/ EM1 / EM2), bold set == 7 (STRONG1
+ all six HEADER levels — multi-slot
bold discipline for header-family
uniformity), highest-defined
`SCE_TXT2TAGS_*` pin
(`SCE_TXT2TAGS_POSTPROC` (25) as top
slot), `L_TXT2TAGS` `LangEntry`'s
`lexer: Some("txt2tags")` + `.t2t`
extension, **header-family cohesion
pin** (all six HEADER slots route to
Keyword AND are in the bold set),
**directive-family cohesion pin**
(OPTION / PREPROC / POSTPROC /
ULIST_ITEM / OLIST_ITEM all route to
Preprocessor), **code-family cohesion
pin** (CODE / CODE2 / CODEBK all
route to String), and
**emphasis-family cohesion pin**
(STRONG1 bold, EM1 italic, EM2
italic — semantic markup styling).

**Visual Prolog (2026-07-08):** wires
`SCLEX_VISUALPROLOG` (= 107, per
`SciLexer.h:123`) for Visual Prolog
(extension `.vip`) — Prolog Development
Center's OOP-flavoured Prolog dialect
with typed classes, interfaces, and
clause bodies. Four-class wordlist
descriptor per `LexVisualProlog.cxx:60-66`
(Major / Minor / Directive / Doc).
Wordlists mirror upstream Lexilla's own
`visualprolog/SciTE.properties` fixture
verbatim: **major** (class 0, 21 tokens
— object model + section declarations +
API-level predicates), **minor** (class
1, 50 tokens — primitive types, control
flow, determinism modes, calling
conventions, Prolog logical operators),
**directive** (class 2, 9 tokens —
`#include` / `#requires` etc. stored
HASHLESS), and **doc** (class 3, 5
tokens — `@short` / `@detail` / `@end`
/ `@exception` / `@withdomain` stored
AT-LESS). 85 total tokens.

**Case-SENSITIVE.** `LexVisualProlog.cxx`
uses `GetCurrent` + `strcmp` throughout,
no lowercasing anywhere. Visual Prolog
is strictly case-sensitive: lowercase-
lead identifiers are atoms/predicates
(IDENTIFIER at `:580`), UPPERCASE-lead
are Prolog variables (VARIABLE at
`:582`), `_`-lead are anonymous
variables (ANONYMOUS at `:584`).
Canonical PDC vocabulary is lowercase-
lead but may contain internal camelCase
for type names (e.g. `binaryNonAtomic`,
`compareResult`, `integerNative`,
`unsignedNative`); invariant 5
enforces lowercase-LEAD + `[a-zA-Z0-9_]`.

**Prefix stripping.** Class 2 entries
hold the HASHLESS stem: source
`#include` parses as OPERATOR `#` +
KEY_DIRECTIVE identifier, then
`directiveKeywords.InList(s + 1)` at
`:429` probes the identifier minus the
leading `#`. Class 3 doc keywords
similarly stored AT-LESS: `@short` in a
comment triggers `docKeywords.InList(s +
1)` at `:461`. Invariant 18 enforces
both prefix strips.

**Cross-class disjointness matrix.**
Classes 0 and 1 share the same call
site at `:411-415` (forward first-match-
wins) — strict disjointness enforced.
Classes 2 (directive) and 3 (doc) are
guarded by distinct entry points
(`#`-lead / `@`-lead), so cross-class
overlap between them and classes 0/1
CANNOT collide at paint time. Per
upstream, `externally` is the single
documented cross-class overlap: it
appears in BOTH class 1 (a modifier
keyword) AND class 2 (`#externally`
compiler directive form). Invariant 7
affirmatively asserts this overlap.

**`end` lookahead.** Special-case at
`:408-410`: when the IDENTIFIER settle
path collects the word `end`,
`endLookAhead` at `:240-253` peeks past
whitespace to the following keyword,
then re-uses `s` to look up THAT
keyword's class. So `end class` paints
as two KEY_MAJOR tokens (because `class`
matches majorKeywords), `end if` paints
as two KEY_MINOR tokens (because `if`
matches minorKeywords). `end` itself is
NOT in class 0 — it lives in class 3
(doc keywords) per upstream, where
`@end` is a block-end marker in
documentation comments. Bare `end` in
code works purely because the following
keyword drives the reclassification.

**Seventeen-mapping style routing** —
25 defined slots minus 8 unmapped:
DEFAULT (framework convention),
IDENTIFIER (transient collect state,
unmatched atoms fall through to
STYLE_DEFAULT), **STRING_ESCAPE_ERROR
(18) deferred to the `StyleSlot::Error`
migration** (authoritative parse
failure — belongs on the same tracked
migration list as Nim's STRINGEOL /
NUMERROR and the ~20+ sibling `_ERROR`
variants; unmapped until the Error
slot lands), and **UNUSED1/2/3 (13-15)
+ UNUSED4 (19) + UNUSED5 (21) are
DEAD STATES** — all five documented as
`"unused"` in `lexicalClasses[]` at
`:92-100` with empty descriptions,
zero call sites verified by exhaustive
grep. Mapped: KEY_MAJOR → Keyword
(bold); KEY_MINOR → Keyword2;
KEY_DIRECTIVE → Preprocessor; two
Comment forms + COMMENT_KEY_ERROR →
Comment (italic); COMMENT_KEY →
Keyword2 (Javadoc-style inside prose);
**VARIABLE / ANONYMOUS → Lifetime**
(Prolog variables are distinctive-
identifier forms — same slot as Rust
`'a` lifetimes / Registry escapes);
NUMBER / OPERATOR / STRING /
STRING_QUOTE / **STRING_EOL → String**
(STRING_EOL is the benign verbatim
multi-line continuation, matching the
7-sibling `_STRINGEOL` → String
convention already established in the
file — not an anomaly marker);
**STRING_ESCAPE → Lifetime** (same
accent as Registry ESCAPED); **EMBEDDED
/ PLACEHOLDER → Macro** — `[| ... |]`
embedded-syntax and `{| ... |}:ident`
placeholder literals are distinctive
special-construct forms.

**Nesting support.** `/* ... */` block
comments nest via `ls.enter(comment)`
at `:587, :451`. `[| ... |]` embedded
syntax nests via `ls.enter(embedded)`
at `:541`. `{| ... |}` placeholders
escape to EMBEDDED when nested inside
an embedded region (`:554-555`) or to
DEFAULT at top-level (`:557`); the
EMBEDDED escape is the more common case
per PLACEHOLDER's `lexicalClasses[]`
description ("in embedded syntax"). The
`kindStack` (2 bits per level, up to 16
levels) supports this at `:322-350`.

Structural test coverage: 23 invariants
— deep-value identity pin, 17-mapping
style count (25 defined slots minus 8
unmapped), four populated classes in
canonical descriptor order, **lowercase-
LEAD + `[a-zA-Z0-9_]` alphabet**
enforcement across every class (accepts
canonical camelCase types like
`binaryNonAtomic`), **strict cross-
class disjointness for classes 0 vs 1**
(load-bearing for forward first-match-
wins), **affirmative overlap assertion
for classes 1 vs 2** (documented
`externally` upstream overlap), style-
routing pins for all 17 mapped
constants, 8 unmapped slots confirmed
absent with drift-pin assertions
(DEFAULT + IDENTIFIER +
STRING_ESCAPE_ERROR + 5 UNUSED slots),
italic set == 4 (all four comment
states), bold set == 1 (KEY_MAJOR —
primary structural anchor), cross-
language non-reuse (C++ / REBOL /
OScript / Spice), `L_VISUALPROLOG`
`LangEntry`'s `lexer: Some("visualprolog")`
+ `.vip` extension, canonical MAJOR /
MINOR / DIRECTIVE / DOC anchors,
**hash/at-prefix-stripped stems only
pin** (LOAD-BEARING — `#`/`@` entries
never match), highest-defined
`SCE_VISUALPROLOG_*` pin
(`SCE_VISUALPROLOG_PLACEHOLDER` (24) as
top slot), **variable-family cohesion
pin** (VARIABLE + ANONYMOUS both
Lifetime), **string-family cohesion
pin** (STRING + STRING_QUOTE +
STRING_EOL all String; STRING_ESCAPE
Lifetime; STRING_ESCAPE_ERROR deferred
to `StyleSlot::Error`), **embedded-
syntax cohesion pin** (EMBEDDED +
PLACEHOLDER both Macro), and no-
duplicate defence-in-depth check.

**TypeScript (2026-07-08):** rides
`LexCPP` (per `L_TYPESCRIPT`'s
`LangEntry` `lexer: Some("cpp")`,
extensions `.ts` / `.tsx`, same
statically-linked `LexCPP.cxx` module
that powers C / C++ / C# / Java /
Objective-C / JavaScript / Resource
file / Swift / Go). Reuses the shared
`CPP_STYLES` / `CPP_ITALIC` /
`CPP_BOLD` table verbatim — only the
class-0 + class-1 keyword pair differs
from JavaScript. **Strict-superset
discipline**: TypeScript is a syntactic
superset of JavaScript at the grammar
level, so every JS reserved word and
every JS built-in constructor must
appear in the corresponding TS
wordlist. The two lists are duplicated
(not cross-referenced) because
`SCI_SETKEYWORDS` takes a flat list
per slot — the invariant test pins
the baseline-superset contract
affirmatively across both classes.

`TYPESCRIPT_KEYWORDS` (class 0, bold —
66 tokens) = the 49-token JS baseline
(ES5 reserved + ES2015+ block-scoped
+ ES2017+ coroutines/for-of + strict-
mode future-reserved + literals) plus
17 TS-specific reserved keywords:
**declaration keywords** (`type` /
`namespace` / `declare` — legacy
`module` deliberately excluded, see
below), **class-member modifiers**
(`abstract` / `readonly` / `override`
/ `accessor`), **type-system
operators** (`is` / `asserts` (TS
3.7+, sibling of `is` for assertion-
function predicates) / `keyof` /
`infer` / `as` / `satisfies` /
`unique` / `intrinsic`), **resource
management** (`using` — TS 5.2+
explicit resource-management with
`using x = disposable` / `await using
x = ...`), and the **variance
annotation** `out` (TS 4.7+; `in`
reuses the JS baseline where it's
already the `in` operator).

`TYPESCRIPT_KEYWORDS_2` (class 1,
accent — 60 tokens) = 9 TS
primitive-type identifiers (`string` /
`number` / `boolean` / `any` / `never`
/ `unknown` / `object` / `symbol` /
`bigint` — the lowercase type-position
spellings distinct from the JS
constructors already in the baseline)
plus the 51-token JS built-ins
baseline from `JAVASCRIPT_KEYWORDS_2`
(general wrappers + concurrent/
iteration primitives + collections +
Error hierarchy + buffer/view + typed
arrays + namespace globals + language/
host globals).

**Deliberate exclusions:** `get` /
`set` / `from` / `global` /
`constructor` / `require` stay out of
class 0 — same identifier-collision
rationale as JavaScript's exclusion
of the same tokens (each is
identifier-shaped in most positions
and highlighting would mis-colour
common user code). **`module`** is
also excluded from class 0: the
legacy TS 1.x `module Foo { ... }`
namespace-declaration syntax was
superseded by `namespace` in TS 1.5,
and including `module` would silently
bold-highlight every `module.exports
= ...` line in the CommonJS idiom
that permeates real-world `.ts`
config / build / Node application
code. `namespace` (its modern
replacement) IS included above.
**`assert`** (deprecated ES2022
import-attributes keyword — replaced
by `with` in ES2024 / TS 5.3) and
**`defer`** (TC39 Stage-3 proposal
for deferred module imports, not
ratified) are also excluded pending
their respective spec outcomes. DOM
instances (`window` / `document` /
`navigator` / `localStorage`), Node
globals (`Buffer` / `process` /
`__dirname`), TS utility types
(`Partial` / `Required` / `Readonly`
/ `Record` / `Pick` / `Omit`), and
framework namespaces (`JSX`) stay
out of class 1 — these are
framework-scope or ambient-declared,
not TS language built-ins.

Structural test coverage: **13
invariants** — style-table reuse pin
(reuses C's `CPP_STYLES` / `CPP_ITALIC`
/ `CPP_BOLD` deep-equal), two-class
descriptor shape (`0 → KEYWORDS`, `1 →
KEYWORDS_2`), **JS-superset
affirmative check** (every
`JAVASCRIPT_KEYWORDS` token verified
present in `TYPESCRIPT_KEYWORDS`, and
every `JAVASCRIPT_KEYWORDS_2` token
verified present in
`TYPESCRIPT_KEYWORDS_2`),
TS-differs-from-JS pins (both class 0
and class 1 must be `assert_ne!` from
JS's — TS adds tokens, so equality
would silently mean the additions
were dropped), **17 TS-specific
class-0 anchors** (`type` /
`namespace` / `declare` / `abstract`
/ `readonly` / `override` /
`accessor` / `is` / `asserts` /
`keyof` / `infer` / `as` /
`satisfies` / `unique` / `intrinsic`
/ `using` / `out`), 9 TS primitive-
type class-1 anchors (`string` /
`number` / `boolean` / `any` /
`never` / `unknown` / `object` /
`symbol` / `bigint`), **9 class-0
exclusion pins** (`constructor` /
`require` / `from` / `get` / `set` /
`global` — identifier-shaped in most
positions — plus `module` for the
CommonJS `module.exports` collision,
plus `assert` (deprecated) and
`defer` (Stage-3 proposal) for spec-
maturity reasons), 13 class-1
exclusion pins (DOM instances + Node
globals + utility types + `JSX`), 5
class-1 value-literal absence pins
(`true` / `false` / `null` /
`undefined` / `void` — all live in
class 0 only, `LexCPP` matches class
0 first so class-1 duplicates would
be dead code), `NaN` + `Infinity`
class-1 presence + class-0 absence
(ES §21.1 Global Value Properties
rationale inherited from JS), and
cross-class disjointness check
(`intersection` must be empty — same
regression net as JavaScript's).

**GDScript (2026-07-08):** rides
Lexilla's `gdscript` lexer
(`LexGDScript.cxx`) — Godot Engine's
scripting language, Python-inspired-
syntax but with its own dedicated
lexer (NOT `LexPython`). Case-
sensitive, byte-exact identifier
match at
`LexGDScript.cxx:459-465`. Godot 4.x
baseline: excludes Godot-3-deprecated
bare keywords (`yield`, `onready`,
`tool`, `remote`, `master`, `puppet`
— all superseded by `await` or
`@`-annotations in Godot 4).

**17 SCE_GD_* states** (0..=16) with
**15-mapping** `GDSCRIPT_STYLES` —
DEFAULT (0) and IDENTIFIER (11)
intentionally unmapped per framework
convention (bare identifiers paint at
STYLE_DEFAULT). Comment-family
collapse: COMMENTLINE (`#`) +
COMMENTBLOCK (`##`) → Comment
italic. **Five string-flavour
collapse**: STRING (`"..."`) +
CHARACTER (`'...'`) + TRIPLE
(`'''..'''`) + TRIPLEDOUBLE
(`"""..."""`) + STRINGEOL → String
(matching the established
_STRINGEOL → String convention
across JS / TypeScript / VHDL / D /
Ada / Verilog / Haskell). WORD
(class-0 hit) → Keyword bold; WORD2
(class-1 hit) → Keyword2 accent.
**Position-derived declaration slots**
CLASSNAME (identifier after `class`)
and FUNCNAME (identifier after
`func`) both → Keyword2 — matches
Python's `SCE_P_CLASSNAME` /
`SCE_P_DEFNAME` and Ruby's
`SCE_RB_CLASSNAME` / `SCE_RB_DEFNAME`
precedent. **`@`-annotations**
(`@onready`, `@export`, `@rpc`,
`@tool`, `@icon`) enter ANNOTATION
only at line-start positions
(`LexGDScript.cxx:594-598`) →
Preprocessor (bold), matching
Python's `SCE_P_DECORATOR` — same
`@name` mechanism, same structural
role. **NodePath sigils** (`$Node/
Path`, `%SceneName`) enter NODEPATH →
Lifetime — structural sigil-tagged
scene-tree reference, same slot
convention as Bash SCALAR / Lisp
SYMBOL / Perl SCALAR.

`GDSCRIPT_KEYWORDS` (class 0, bold —
35 tokens): 12 control-flow (`if`
/ `elif` / `else` / `for` / `while`
/ `break` / `continue` / `pass` /
`return` / `match` / `when` /
`breakpoint`), 10 declaration (`var`
/ `const` / `func` / `class` /
`class_name` / `enum` / `signal` /
`extends` / `static` / `abstract`),
6 keyword-operators (`and` / `or` /
`not` / `in` / `is` / `as`), 1
coroutine (`await`), 3 special
identifiers (`self` / `super` /
`void`), 3 literals (`true` /
`false` / `null`). **`class_name`
is ONE compound token** — Godot's
script-global class declarator, not
`class` + `_name`.

`GDSCRIPT_KEYWORDS_2` (class 1,
accent — 132 tokens): 3 primitive
types (`bool` / `int` / `float`),
35 Variant types (`String`,
`StringName`, `NodePath`,
`Callable`, `Signal`, `Dictionary`,
`Array`, `Rect2` / `Rect2i`,
`Vector2` / `Vector2i` / `Vector3`
/ `Vector3i` / `Vector4` /
`Vector4i`, `Transform2D` /
`Transform3D`, `Plane`,
`Quaternion`, `AABB`, `Basis`,
`Color`, `RID`, `Object`, and the
Godot 4 typed-packed-array family
`PackedByteArray` /
`PackedInt32Array` /
`PackedInt64Array` /
`PackedFloat32Array` /
`PackedFloat64Array` /
`PackedStringArray` /
`PackedVector2Array` /
`PackedVector3Array` /
`PackedVector4Array` /
`PackedColorArray`, plus `Variant`
universal wrapper), 4 mathematical
constants (`PI` / `TAU` / `INF` /
`NAN`), 8 printing / diagnostics,
5 conversion/control (`str` /
`range` / `preload` / `load` /
`assert`), and ~77 Global Scope
built-in functions (Godot's
`@GDScript` / `@GlobalScope`
utility surface: absolute value /
rounding + typed int/float
variants, range clamping,
power/log/modulo, trigonometry,
angle conversion, interpolation,
random, predicates, introspection/
serialisation, wrap).

**Deliberate exclusions:** `get` /
`set` (property-accessor
contextual keywords) stay out of
class 0 to avoid mis-colouring
Dictionary `.get()` / Array
`.set()` method calls. Godot-3-
deprecated bare `yield` /
`onready` / `tool` / `remote` /
`master` / `puppet` / `slave` /
`remotesync` / `mastersync` /
`puppetsync` stay out — all became
`@`-annotations in Godot 4 (handled
by SCE_GD_ANNOTATION). Node
instance methods (`add_child` /
`queue_free` / `get_node` /
`_ready` / `_process` /
`_physics_process`) stay out of
class 1 — they're inherited on
Node, not Global Scope, and would
mis-colour user object methods of
the same name. Engine singletons
(`Input` / `OS` / `Engine` /
`Time` / `Performance` /
`ProjectSettings` /
`RenderingServer` /
`PhysicsServer2D` /
`PhysicsServer3D` /
`DisplayServer`) stay out —
deliberately excluded per
framework-specific-dynamic-set
rationale, since the singleton set
churns across Godot 4.x minor
versions.

Structural test coverage: **16
invariants** — deep-value identity
pin (`styles` / `italic` / `bold`
value-equal `GDSCRIPT_STYLES` /
`GDSCRIPT_ITALIC` / `GDSCRIPT_BOLD`),
15-mapping style count, two-class
descriptor shape (matches
`gdscriptWordListDesc[]` at
`LexGDScript.cxx:171-175`),
canonical class-0 + class-1
wordlist links, exact style-routing
pin for all 15 mapped constants,
DEFAULT + IDENTIFIER unmapped drift
check, five-string-flavour cohesion
pin (all → String), two-comment
cohesion pin (both → Comment),
CLASSNAME + FUNCNAME declaration-
slot cohesion pin (both →
Keyword2), italic == 2 comment
states, bold == 2 (WORD +
ANNOTATION — matches Python's
`SCE_P_WORD` + `SCE_P_DECORATOR`
bold pair), **`class_name`
one-compound-token** presence,
strict class-0 vs class-1
disjointness (LexGDScript probes
class 0 first at `:459` — a
duplicate in class 1 is dead code),
**Godot-4-specific exclusion pins**
(10 deprecated bare keywords + 2
property-accessor contextuals in
class 0; 6 Node instance methods +
10 engine singletons in class 1),
20 canonical class-0 anchors and
21 canonical class-1 anchors, and
cross-language non-reuse check
(`GDSCRIPT_STYLES` distinct from
`PYTHON_STYLES`).

**Phase 4.5 gate retained.** Per
DESIGN.md §7.2's `Normal Text` (⚫
by design) exclusion from the
percentage, coverage sits at ✅ 72
/ 88 wired-eligible rows (81.8%)
after this commit — comfortably
past the ≥80% Phase 4.5 completion
gate. The gate was actually crossed
by the preceding TypeScript commit
at ✅ 71 / 88 = 80.7%; this
GDScript commit keeps coverage
past the gate with a further
1.1-point margin. The residual 🟡
rows continue to be tracked for
follow-on commits.

**Hollywood (2026-07-08):** rides
Lexilla's `hollywood` lexer
(`LexHollywood.cxx`) — Andreas
Falkenhahn's proprietary Lua-
inspired multimedia programming
language (extension `.hws`). Case-
INSENSITIVE identifier lookup via
`GetCurrentLowered` at
`LexHollywood.cxx:357` — Hollywood's
source convention is PascalCase
(`Print`, `LoadBrush`) but every
wordlist entry MUST be stored
lowercase for the byte-exact InList
match against the lowercased input.

**15 SCE_HOLLYWOOD_* states**
(0..=14) with **13-mapping**
`HOLLYWOOD_STYLES` — DEFAULT (0)
and IDENTIFIER (12) intentionally
unmapped per framework convention.
**Numeric-family collapse**: NUMBER
+ HEXNUMBER (both `$abc` and
`0xabc` prefixes) → Number.
**Comment-family collapse**: `;`-
line-comment (COMMENT) + `/*..*/`
block-comment (COMMENTBLOCK) both
→ Comment italic. **String-family
collapse**: `"..."` (STRING) + Lua-
style `[[..]]` heredoc
(STRINGBLOCK) both → String. **API-
family collapse**: STDAPI (class 1
stdlib) + PLUGINAPI (class 2
plugin globals) + PLUGINMETHOD
(class 3 plugin methods) all three
→ Keyword2 accent, matching the
LexCPP-family SCE_C_WORD2 accent-
slot convention. KEYWORD (class 0)
→ Keyword bold. PREPROCESSOR
(`@REQUIRE` / `@INCLUDE` / `@VERSION`
/ `@DISPLAY`) → Preprocessor bold —
matches Python's `SCE_P_DECORATOR`
precedent (identical `@name`
mechanism). CONSTANT (`#RED` /
`#WHITE` / `#TRUE` named-constant
sigils) → Lifetime — structural-
sigil-tagged reference matching the
established Bash SCALAR / Lisp
SYMBOL / Perl SCALAR / GDScript
NODEPATH precedent.

**Last-match-wins across classes.**
The `for i in 0..4` loop at
`LexHollywood.cxx:358-362` does NOT
break — it keeps probing every
class and each `ChangeState`
overwrites the previous match. A
token appearing in both class 0 and
class 1 silently promotes to STDAPI
colour. Same discipline as REBOL
(REBOL_WORD..REBOL_WORD8 last-match-
wins). Framework consequence:
strict cross-class disjointness
enforced by the invariant test's
`HashSet::intersection` guard.

`HOLLYWOOD_KEYWORDS` (class 0,
bold — 39 tokens, all-lowercase) —
verified against Hollywood 11.0
manual chapters 11 (variables/
constants) and 12 (control flow):
5 conditional (`if` / `then` /
`else` / `elseif` / `endif`), 4
switch (`switch` / `case` /
`default` / `endswitch`), 12 loop
(`for` / `to` / `step` / `next` /
`in` / `do` — short-form signal —
/ `while` / `wend` / `repeat` /
`until` / `forever` /
`break`), 2 jump (`return` /
`continue` — `goto` deliberately
excluded as Hollywood 1.x legacy
per the manual's own deprecation
note), 9 declaration (`function` /
`endfunction` / `local` / `global`
/ `const` / `dim` / `dimstr` —
array declarations — / `block` /
`endblock` — scoping statement),
6 boolean/literal (`and` / `or` /
`not` / `true` / `false` /
`nil`).

**Deliberately absent from class
0** (adversarial review found and
removed): `foreach` is NOT a
Hollywood keyword — the generic
loop form is `For var In expr`;
`ForEach(table, callback)` exists
but as a Table-library function
(lives in `HOLLYWOOD_STDAPI` class
1). `forrange` and `enum` don't
exist in Hollywood at all.

`HOLLYWOOD_STDAPI` (class 1,
accent — 96 tokens, all-lowercase)
— conservatively verified against
Hollywood 11.0 manual chapter TOCs.
Favours **verifiable correctness
over API-surface coverage**:
Hollywood's full API is ~600
functions across ~40 libraries;
this list is a ~96-token subset
verified against the manual.
Function-name guesses that could
plausibly exist (`getdisplaywidth`,
`pauseanim`, `getanimcount`, etc.)
are deliberately omitted where the
manual doesn't confirm them — an
incorrectly-named entry paints
harmlessly as
`SCE_HOLLYWOOD_IDENTIFIER`, but a
correctly-named-BUT-wrong-name
entry mis-highlights real user
code.

Categories (17): 4 console I/O
(`print` / `debugprint` / `nprint`
— Print-without-newline, NOT
`printnln` — / `cls`), 7 file I/O
(bare `seek` and `exists`, NOT
`fileseek`/`fileexists`), 2 file
requester + existence, 4 display
(bare `settitle`, NOT
`setdisplaytitle`), 5 brush, 4
sprite, 4 animation, 6 music
(Pause/Resume verbs exist HERE,
not for Sample), 5 sample
(Load/Create/Free lifecycle — NOT
Open/Close), 7 font + text (7
because `nprint` is in category 1),
2 colour (`rgb` + `argb`), **20
math (fully verified)**, **10
string (fully verified)**, 5 type
conversion + Table (`getitem` NOT
`gettable`), 2 table iteration
(`foreach` as function, `sort` NOT
`sorttable`), 5 time, 4 random +
events (`rndseed` NOT `srand`).

**Deliberately absent from class 1**
(adversarial review found and
removed as fabricated / misnamed):
`input`, `fileseek`, `fileexists`,
`flushdisplay`, `getdisplayattributes`,
`setdisplayattributes`,
`getdisplaywidth`, `getdisplayheight`,
`getdisplaycount`, `setdisplaytitle`,
`savebrush`, `scalebrush`,
`rotatebrush`, `getbrushwidth`,
`getbrushheight`, `pauseanim`,
`getanimwidth`, `getanimheight`,
`getanimcount`, `opensample`,
`closesample`, `pausesample`,
`resumesample`, `printnln`,
`getrgb`, `setcolor`, `gethexcolor`,
`gettable`, `settable`,
`gettablesize`, `cleartable`,
`sorttable`, `srand`. The invariant
test asserts each absent so a
future regression that re-adds any
of them fails the gate.

**Deliberate exclusions:** Plugin-
provided globals (`hurl.request`,
`hcl.compress`, `xmlparser.parse`,
`sqlite.open` — from user's plugin
set) stay out — belong in class 2
(PLUGINAPI), left empty per
framework-specific-dynamic-set
convention. Plugin object methods
(`sprite:move`, `brush:copy`) stay
out — belong in class 3
(PLUGINMETHOD), also left empty.
Named constants (`#RED` / `#WHITE`
/ `#TRUE`) stay out — they aren't
wordlist-classified at all; the
lexer enters `SCE_HOLLYWOOD_CONSTANT`
state on `#` and paints the whole
`#name` span without a wordlist
lookup. `@`-preprocessor directives
same story — `SCE_HOLLYWOOD_PREPROCESSOR`
paints them without wordlist lookup.

Structural test coverage: **14
invariants** — deep-value identity
pin, 13-mapping style count, **two-
class descriptor shape** (Code++
populates class 0 + class 1;
classes 2 + 3 left empty per
framework-specific-dynamic-set
convention — invariant asserts
`keywords.len() == 2`, not 4),
canonical class-0 + class-1
wordlist links, exact style-routing
pin for all 13 mapped constants,
DEFAULT + IDENTIFIER unmapped drift
check, numeric-family collapse pin
(NUMBER + HEXNUMBER → Number),
string-family collapse pin (STRING
+ STRINGBLOCK → String), **API-
family collapse pin** (STDAPI +
PLUGINAPI + PLUGINMETHOD three-way
collapse to Keyword2), comment-
family collapse + italic pin (both
COMMENT + COMMENTBLOCK italic;
`italic.len() == 2`), CONSTANT →
Lifetime + PREPROCESSOR →
Preprocessor sigil-state pins,
bold set == 2 (KEYWORD +
PREPROCESSOR), **all-lowercase
wordlist enforcement** (both
wordlists scanned for any uppercase
byte — LOAD-BEARING for
`GetCurrentLowered` case-
insensitive matching), strict
cross-class disjointness
(`HashSet::intersection` empty —
last-match-wins classifier would
silently promote duplicates to
STDAPI), 30 canonical class-0
anchors + 20 canonical class-1
anchors.

**Go (2026-07-08):** rides `LexCPP`
(per `L_GOLANG`'s `LangEntry`
`lexer: Some("cpp")`, extension
`.go`) — reuses the shared
`CPP_STYLES` / `CPP_ITALIC` /
`CPP_BOLD` table verbatim, same as
C / C++ / C# / Java / Objective-C /
JavaScript / TypeScript / Resource
file / Swift. Only the class-0 +
class-1 keyword pair differs.

**Closed reserved-word set.** Go's
language spec §"Keywords"
explicitly enumerates all 25
reserved words, and no additions
have been made in any 1.x release
since Go 1.0 (December 2011). The
invariant test pins exact count
(25 spec-reserved + 4 predeclared-
literal editorials = 29 class-0
tokens) so any future edit adding
a spurious token trips loudly.

`GO_KEYWORDS` (class 0, bold — 29
tokens): the 25 spec reserved
words `break` / `case` / `chan` /
`const` / `continue` / `default` /
`defer` / `else` / `fallthrough` /
`for` / `func` / `go` / `goto` /
`if` / `import` / `interface` /
`map` / `package` / `range` /
`return` / `select` / `struct` /
`switch` / `type` / `var`, plus 4
predeclared literals (`true` /
`false` / `nil` / `iota`). The
literals are strictly speaking
predeclared identifiers per spec
§"Predeclared identifiers" (they
live in the universe block, not
the reserved-words list) — placed
in class 0 per editorial
convention, matching every
mainstream Go styler (the Go
Playground, Goland default, VS
Code Go extension, `SciTE`'s
`go.properties`, Notepad++ stock).
Same editorial-placement
discipline as
`JAVASCRIPT_KEYWORDS`'s `null` /
`undefined`.

`GO_KEYWORDS_2` (class 1, accent —
40 tokens): 20 predeclared
primitive types (`bool` / `byte` /
`complex64` / `complex128` /
`error` — the built-in interface
type — / `float32` / `float64` /
`int` / `int8` / `int16` / `int32`
/ `int64` / `rune` — alias for
`int32` — / `string` / `uint` /
`uint8` / `uint16` / `uint32` /
`uint64` / `uintptr`), 2 Go 1.18+
generic-typing predeclared
identifiers (`any` — alias for
`interface{}` — and `comparable`
— the constraint interface), 18
built-in functions (`append` /
`cap` / `clear` (1.21+) / `close`
/ `complex` / `copy` / `delete` /
`imag` / `len` / `make` / `max`
(1.21+) / `min` (1.21+) / `new` /
`panic` / `print` / `println` /
`real` / `recover`).

**Deliberate exclusions:**
standard-library package names
(`fmt` / `os` / `io` / `sync` /
`context` / `http` / `json` /
`strings` / `strconv` / `bytes` /
`errors`) stay out of both classes
— Go's stdlib surface is ~200
packages with thousands of
exported identifiers; users get
IDE-style stdlib completion from
`gopls` (Go's LSP), not from a
lexer wordlist. User-convention
identifiers (`err` / `ok` / `i` /
`s`) stay out — they're plain
identifiers, not language-
reserved. The blank identifier `_`
tokenises as a plain identifier by
`LexCPP` and never sees the
wordlist path.

Structural test coverage: **the
LexCPP-family test pattern** —
style-table reuse pin (reuses C's
`CPP_STYLES` / `CPP_ITALIC` /
`CPP_BOLD` deep-equal), two-class
descriptor shape (`0 → KEYWORDS`,
`1 → KEYWORDS_2`), canonical
wordlist links, **exact class-0
count == 29** (25 spec + 4
predeclared literals), 25
spec-reserved-word anchors
verified by name, 4 predeclared-
literal anchors (`true`/`false`/
`nil`/`iota` in class 0), 20
predeclared-type anchors in class
1, 2 Go 1.18+ generic-typing
anchors (`any` / `comparable`),
18 built-in-function anchors in
class 1 (including Go 1.21+
additions `clear` / `max` /
`min`), strict cross-class
disjointness (`intersection`
empty — `LexCPP` probes class 0
first at `:995-999`, duplicates
would be dead code), 8 class-0
exclusion pins (predeclared
types / built-in functions must
NOT leak into class 0), 11
stdlib-package exclusion pins,
4 user-convention identifier
exclusion pins, and 6 cross-
language divergence pins
(GO_KEYWORDS / GO_KEYWORDS_2
must not equal JS / TS / Java
class-0 or class-1 lists).

**Raku (2026-07-08):** rides
Lexilla's `raku` lexer
(`LexRaku.cxx`) — the current name
of what was previously called "Perl
6". A gradually-typed, object-
oriented / functional /
declarative language with rich
lexical syntax: sigils
(`$`/`@`/`%`/`&` scalars /
positionals / associatives /
callables), phasers
(`BEGIN`/`END`/`ENTER`/`LEAVE`/
`CATCH`/etc.), the **Q language**
(`q`/`qq`/`Q`/`qw`/`qww` string-
quoting families with adverbs like
`:to` for heredoc / `:i` for case-
insensitive / `:g` for global),
POD documentation blocks, regexes
with `:i`/`:g` adverbs, and
grammars (Raku's built-in PEG
parser DSL). Extensions `.raku` /
`.rakumod`.

**Seven-class wordlist descriptor**
— the richest wordlist install in
Phase 4.5. `LexRaku.cxx:106-115`
declares seven named classes:
class 0 "Keywords and identifiers"
(reserved words + phasers), class
1 "Functions" (built-in-function
API), class 2 "Types basic"
(primitives + Cool/Any/Mu
hierarchy), class 3 "Types
composite" (Array/Hash/List/Set/
Bag containers), class 4 "Types
domain-specific" (I/O /
concurrency / grammar / POD
types), class 5 "Types exception"
(the `X::` hierarchy), class 6
"Adverbs" (`:sym`/`:qq`/`:to`/
`:heredoc` regex + Q-language
modifiers, stored without the `:`
prefix and gated by
`LexRaku.cxx:1400-1407`'s `:`
check).

**Contents mirror the upstream
Lexilla test fixture** at
`crates/scintilla-sys/vendor/
lexilla/test/examples/raku/
SciTE.properties` verbatim — Code
++'s wordlists are a
mirror-not-curate reproduction of
the Lexilla project's authoritative
fixture. Rationale: Raku's spec is
evolving (new phasers, control-
flow keywords added in 6.d / 6.e /
6.f language versions) and
delegating to upstream guarantees
no drift. One documented fixture
gap flagged in the invariant test:
`reduce` is NOT in the class-1
fixture despite being a common
Raku function (upstream inherits
its function list from Perl 5's
stock and hasn't been kept in
perfect sync with Raku 6.d's full
API). `sub` (subroutine
declarator) IS in class 1 — it
paints as `SCE_RAKU_FUNCTION`
accent, NOT bold Keyword.
`say` also IS in class 1
(verified against the fixture).

**29 SCE_RAKU_* states** (0..=28)
with **26-mapping** `RAKU_STYLES`
— DEFAULT (0), ERROR (1), and
IDENTIFIER (21) intentionally
unmapped per framework convention
+ deferred `StyleSlot::Error`
migration. **Case-SENSITIVE
lookup** (`GetCurrent` not
`GetCurrentLowered`) — Raku
convention: types are `PascalCase`
(`Str` / `Int` / `IO::Handle`),
keywords + functions are lowercase
(`if` / `for` / `abs`), phasers +
declarative-scope markers are
`SCREAMING` (`BEGIN` / `END` /
`CATCH` / `LEAVE`).

**Three-family comment collapse**:
COMMENTLINE (`#`) + COMMENTEMBED
(`#|` / `#=` declarator-doc) + POD
(`=begin pod` / `=end pod`) all →
Comment italic. **Eight-flavour
string collapse** — richest string
collapse in Phase 4.5: CHARACTER +
HEREDOC_Q + HEREDOC_QQ + STRING +
STRING_Q + STRING_QQ +
STRING_Q_LANG + STRING_VAR all →
String. **Regex-family collapse**:
REGEX + REGEX_VAR both → String
(Perl / Bash regex convention).
**Sigil-family four-way collapse**:
MU (`$scalar`) + POSITIONAL
(`@array`) + ASSOCIATIVE (`%hash`)
+ CALLABLE (`&code`) all →
Lifetime, matching Bash SCALAR /
Lisp SYMBOL / Perl SCALAR /
GDScript NODEPATH structural-sigil
precedent. **Declaration-slot
collapse**: GRAMMAR + CLASS both →
Keyword2 (position-derived
identifiers after `grammar` /
`class` keywords, matching Python
`SCE_P_CLASSNAME` /
`SCE_GD_CLASSNAME` precedent).
**Type-family collapse**: TYPEDEF
is the SINGLE style slot for
classes 2-5 wordlist hits
(upstream collapses all four
TYPEDEF wordlists at
`LexRaku.cxx:1373-1380`). WORD
(class 0) → Keyword bold;
FUNCTION (class 1) → Keyword2;
ADVERB (class 6) → Keyword2;
PREPROCESSOR (`use v6.d` / `use
MONKEY-TYPING`) → Preprocessor
bold.

Structural test coverage: **14
invariants** — deep-value identity
pin, 26-mapping style count,
seven-class descriptor shape,
canonical class-N wordlist links
(all 7 classes), exact style-
routing pin for all 26 mapped
constants, DEFAULT + ERROR +
IDENTIFIER drift check, comment-
family + string-family (8-flavour)
+ regex-family + **sigil-family
(4-way)** + declaration-slot
collapse pins, TYPEDEF single-slot
pin, italic == 3 + bold == 2, and
canonical anchors per class
(`if`/`for`/`class`/`BEGIN`/`END`
in KEYWORDS, `abs`/`print`/`push`/
`sort`/`map`/`grep` in FUNCTIONS,
`Str`/`Int`/`Mu` in TYPES_BASIC,
`Array`/`Hash`/`List`/`Set`/`Bag`
in TYPES_COMPOSITE, `IO::Handle`/
`Promise`/`Channel`/`Grammar` in
TYPES_DOMAIN, `X::AdHoc`/
`X::TypeCheck`/`Exception` in
TYPES_EXCEPTION, `sym`/`to`/`qq`/
`heredoc`/`words` in ADVERBS —
stored without leading `:`).

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
