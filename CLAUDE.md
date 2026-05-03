# Project: Code++

## What This Project Does

Code++ is a 1:1, cross-platform clone of Notepad++ written in Rust on top of Scintilla, with binary-compatible support for existing Notepad++ plugins on Windows. The core engineering goal is **Notepad++-class startup and editing performance on every platform** — achieved by aggressive architectural choices, not micro-optimization: a headless `core`, a thin native UI per platform, statically linked Scintilla + Lexilla, and Scintilla's direct-call function pointer on every hot path.

## Stack

- **Language:** Rust (stable, pinned via `rust-toolchain.toml`); vendored C/C++ for Scintilla 5.x + Lexilla 5.x.
- **Build:** Cargo workspace; `cc` crate compiles vendored Scintilla source via `crates/scintilla-sys/build.rs`.
- **Test:** `cargo test` (unit tests in `core`, FFI smoke tests in `scintilla-sys` and `editor`, plugin compat harness in `tools/npp-plugin-test`).
- **Key deps:** `windows` (Win32), `gtk4-rs` (Linux), `objc2` (macOS), `notify` (file watching), `crossbeam-channel` (cross-thread marshaling), `tracing`, `quick-xml`.

## Directory Layout

```text
crates/core/         → headless logic: session, file I/O, encoding, EOL, settings
crates/scintilla-sys → custom FFI crate; vendors Scintilla + Lexilla as submodules
crates/editor/       → safe Scintilla wrapper; owns the direct-call function pointer
crates/platform/     → OS utilities (paths, dynlib, file watch)
crates/plugin-host/  → NPPM/NPPN dispatcher; loads N++-compatible DLLs
crates/shell/        → glue + UiPlatform trait; owns Session and EditorHandles
crates/ui_win32/     → Win32 UI; ui_gtk and ui_cocoa added in Phase 5
crates/app/          → thin binary; selects UI backend via cargo features
plugins/             → in-tree sample plugin + N++-compat headers
tools/               → plugin compat harness, perf scripts
```

## Essential Commands

```powershell
git submodule update --init --recursive   # required: Scintilla + Lexilla source
cargo build --workspace                   # builds everything
cargo run -p app                          # launches Code++ (current phase's demo)
cargo test --workspace                    # runs all tests
cargo fmt --check                         # must pass
cargo clippy --workspace -- -D warnings   # must pass
```

## Project-Specific Rules

These are not derivable from the code. Read DESIGN.md for the rationale; these are the operational rules.

- **The phase rule (DESIGN.md §7.1) is binding.** No phase ships without its end-of-phase **Demo** running end-to-end against real Scintilla, real Win32/GTK/Cocoa events, real disk I/O, and real loaded plugin DLLs. Stubs/mocks in the integration path are forbidden — only `core` unit tests may use them.
- **Performance constraints (DESIGN.md §8) are non-negotiable.** Cold start < 80 ms, keystroke p99 < 5 ms, empty-buffer RSS < 25 MB. A change that regresses any constraint by > 20% blocks the phase, regardless of feature value. Feature-gate or redesign instead.
- **Hot paths use Scintilla's direct-call API**, never `SendMessage`. The `(fn_ptr, instance_ptr)` pair is captured once per Scintilla control and stored on `EditorHandle`. `SendMessage` is only for setup and cross-thread one-shots.
- **Dependency direction is strictly downward** (DESIGN.md §2.1). `core` is headless — no `windows`, `gtk4`, `objc2`, no Scintilla, no UI types. Adding any of those to `core` is a design break.
- **Workers never touch `HWND`s or Scintilla state directly.** Use the marshaling pattern in DESIGN.md §5.4: typed message on a channel + `PostMessage(WM_APP_WAKE)` (or platform equivalent).
- **Plugin ABI freezes at Phase 3 completion** and never breaks. The NPPM/NPPN message set is documented in `docs/nppm-coverage.md`; new messages may be added, existing ones may not change shape. Breaking N++ binary compat on Windows is a release-blocking bug.
- **Static linking only.** Scintilla, Lexilla, and CRT are statically linked into the `code++` binary. No shipped `scintilla.dll`. Plugin DLLs are the only dynamic loads.
- **Plugins load lazily.** Zero `LoadLibrary`/`dlopen` calls happen at startup; verified by trace log.
- **Pre-commit gate (binding).** Before every commit, run `/review` and `/security-audit` and address all findings — fix them, or get explicit user sign-off to defer with a tracked follow-up. No commit ships with unresolved findings from either skill. This applies to every commit, including small ones; the FFI surface and plugin host make "obviously safe" the wrong default.
- **Final fmt pass before staging (binding).** The very last action before `git add` for any commit is `cargo fmt --all && cargo fmt --all -- --check`. The `--check` is the assertion that the working tree matches what CI's `cargo fmt --all -- --check` will run on. fmt run earlier in the gate is **not** sufficient — every subsequent edit invalidates that snapshot. Reason: rustfmt 1.95+ is layout-aware (will collapse a multi-line call onto one line if it fits 100 cols), so an edit between gate and commit can produce a diff CI rejects even when no semantics changed.
- **Dependency licenses.** Code++ is MIT (see [LICENSE](LICENSE)). Cargo dependencies must use a license from the allowlist in `deny.toml` (MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, 0BSD, CC0-1.0, Unicode-DFS-2016, Unicode-3.0). Copyleft (GPL/LGPL/MPL/AGPL) is denied by omission — find a different crate, never add an exception. `cargo deny check` runs in CI and is also expected before any commit that touches `Cargo.toml`/`Cargo.lock`.
- **No code from Notepad++.** The plugin compat headers under `plugins/nppcompat-headers/` are clean-room reimplementations of the public ABI. Pasting source from Notepad++ headers is a license-violation bug; revert and rewrite. Every header in that directory must start with the provenance notice in `plugins/nppcompat-headers/HEADER_TEMPLATE.txt`.

## Skills Available

- `codebase-navigator` — use when first exploring this repo.
- `code-quality` — use before committing any changes.
- `review` — use to review changes before commit.

## See Also

@README.md
@DESIGN.md
@DEVELOPMENT.md
