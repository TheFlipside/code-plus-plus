# Notepad++ plugin compatibility headers

Independent, clean-room reimplementation of the Notepad++ plugin ABI.
Plugins that include these headers and compile to a Win32 DLL load
into Code++ on Windows unchanged. The same source — recompiled to
`.so`/`.dylib` — runs in Code++ on Linux/macOS once Phase 5 lands the
non-Windows backends.

## What's here

| File | Purpose |
| --- | --- |
| [`PluginInterface.h`](PluginInterface.h) | The six required entry points (`setInfo`, `getName`, `getFuncsArray`, `beNotified`, `messageProc`, `isUnicode`), plus `NppData`, `FuncItem`, `ShortcutKey`. |
| [`Notepad_plus_msgs.h`](Notepad_plus_msgs.h) | All `NPPM_*` host-control messages and `NPPN_*` notification codes. |
| [`menuCmdID.h`](menuCmdID.h) | Built-in menu command IDs plugins reference via `NPPM_MENUCOMMAND`. |
| [`HEADER_TEMPLATE.txt`](HEADER_TEMPLATE.txt) | The provenance comment block every header in this directory must start with. |

## Clean-room rule (binding, see [CLAUDE.md](../../CLAUDE.md))

The headers above are independent reimplementations of the public ABI.
Every numeric constant, struct field name, and function signature
matches Notepad++'s public ABI by design (the ABI is not copyrightable
per *Google v. Oracle*; the original header source is). **No source
text has been copied from Notepad++ or its plugin SDK.**

Adding a new header here:

1. Copy `HEADER_TEMPLATE.txt` and fill in the placeholders.
2. Look up the constants / struct shapes you need from public
   documentation, third-party plugin tutorials, and the behavior
   contract — never the upstream `.h` source.
3. Write the declarations from scratch in the same style as the
   existing headers in this directory.
4. If a declaration cannot be written without referencing the upstream
   source verbatim, stop and ask.

A PR that pastes from Notepad++ headers is a license-violation bug,
not a style preference. It gets reverted, not landed-and-fixed-later.

## Coverage

Code++ stages NPPM coverage across phases:

- **Phase 3 (v1):** the messages tagged `v1` inline in
  [`Notepad_plus_msgs.h`](Notepad_plus_msgs.h) — the minimum set that
  lets a small Notepad++ plugin (e.g. one that inserts text into the
  active buffer) load and function unchanged.
- **Phase 4:** find-in-files, encoding-conversion, and language-type
  messages.
- **Phase 5:** the long tail.

The authoritative coverage matrix lives in
[`docs/nppm-coverage.md`](../../docs/nppm-coverage.md) (added when
Phase 3's plugin-host crate lands).

A plugin can `#include` this entire header set today, regardless of
which `NPPM_*` Code++ has wired up. Sending an unimplemented message
returns 0 and emits a `tracing::warn!` on the host side, so plugins
*always link* — Code++ surfaces missing coverage at runtime, not at
plugin-build time.

## Building a plugin against these headers

Plugin authors need three include paths in their build:

1. This directory (`plugins/nppcompat-headers/`) — for `PluginInterface.h`,
   `Notepad_plus_msgs.h`, `menuCmdID.h`.
2. The vendored Scintilla `include/` directory
   (`crates/scintilla-sys/vendor/scintilla/include/`) — for
   `Scintilla.h`, `ScintillaTypes.h`, `ScintillaMessages.h`.
3. The Win32 SDK (already present in any MSVC / clang-cl install).

The in-tree sample plugin at [`plugins/example-hello/`](../example-hello/)
(added when Phase 3's plugin-host milestone lands) sets these paths
up automatically via `build.rs` and serves as the reference build
configuration.
