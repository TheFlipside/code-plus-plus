# Code++

**A 1:1, cross-platform clone of Notepad++ — written in Rust, powered by Scintilla, fast on every OS.**

<img src="assets/code++.png" alt="Code++" width="128">

---

## What It Is

Code++ is what Notepad++ would be if it shipped on Linux and macOS without compromising on what made it great on Windows: a fast cold start, instant typing, modest memory use, and a thriving plugin ecosystem.

The promise is concrete:

- **Same UX as Notepad++.** Tabs, sessions, encoding control, EOL control, find-in-files, syntax highlighting via Scintilla's lexers — the keyboard shortcuts and menus you already know.
- **Same plugin ecosystem.** Existing Notepad++ plugin DLLs load into Code++ on Windows unchanged. The same plugin source compiles to `.so`/`.dylib` on Linux and macOS.
- **Same performance class.** Cold start under 80 ms, keystroke latency under 5 ms p99, empty-buffer footprint under 25 MB. These are not aspirations — they are constraints enforced at every phase boundary.
- **Truly cross-platform.** Native Win32, GTK 3, and Cocoa front-ends over a single Rust core. No Electron. No browser engine. No GC runtime.

---

## Why Build This

Notepad++ is excellent and Windows-only. Every "cross-platform Notepad++ alternative" trades performance for portability — usually by wrapping a web view, embedding a JavaScript runtime, or layering a heavy framework. Code++ rejects that trade.

The thesis: **performance is an architectural property, not a tuning result.** You get Notepad++-class speed by making the right choices up front — a headless core, statically linked Scintilla, the direct-call API for hot paths, lazy plugin loading, and zero startup work that isn't strictly required to paint the first frame. Once those decisions are in place, the speed follows. Once they are violated, no amount of profiling brings it back.

---

## Core Ideas

1. **Scintilla does what Scintilla does best.** We use Scintilla 5.x and Lexilla 5.x for the editing surface, undo/redo, search, and syntax highlighting. We do not reimplement them. We compile them statically into the binary so there is no DLL hell and no loader-time penalty.
2. **The core is headless.** `crates/core` knows about sessions, files, encodings, and EOL — and nothing about windows, HWNDs, GTK widgets, or Scintilla. It is unit-testable without an OS event loop. UI crates depend on `core`; `core` depends on no one.
3. **Direct-call on every hot path.** Scintilla's `SCI_GETDIRECTFUNCTION` returns a function pointer that bypasses the window message pump. Code++ captures it once per editor and routes every keystroke, insert, and selection through it. `SendMessage` is reserved for setup and cross-thread one-shots.
4. **Zero work at startup that isn't on the visible-frame critical path.** No directory scans. No plugin DLL loads. No project indexing. No AST construction. The session file is read; windows are created; threads are spawned for whatever needs to happen later. That's it.
5. **Plugins load when they are first used, not before.** A user with 40 installed plugins pays no startup cost for the 39 they don't touch this session.
6. **Notepad++ plugin compatibility from the ground up.** The six plugin entry points, the `NppData` struct, and the `NPPM_*`/`NPPN_*` message families are implemented to match Notepad++'s public ABI. A binary plugin that works in Notepad++ on Windows works in Code++ on Windows.
7. **End-to-end demos at every phase.** Every implementation phase (see [DESIGN.md](docs/DESIGN.md) §7) ends with a runnable demo against real Scintilla, real OS events, real disk I/O, and real plugin DLLs. No phase ships on stubs. This rule exists to stop architectural drift before it starts.

---

## Project Status

**Phase 0 — Scaffolding** is the current phase. The repository contains the design and developer-environment documentation; the workspace, crates, and submodule pins land next. See [DESIGN.md](docs/DESIGN.md) §7.2 for the full phase plan and end-of-phase demo for each.

| Phase | Theme | Demo |
| --- | --- | --- |
| 0 | Workspace + CI green | Empty Win32 window opens and closes |
| 1 | Scintilla shell | Real Scintilla control, type/undo/redo work |
| 2 | Core: session, files, encoding | Open/save/restore real files, no UI freeze |
| 3 | Multi-tab + plugin host | Real N++ plugin DLL loads and runs |
| 4 | Lexers, search, find-in-files | Highlighting + search at scale |
| 4.5 | Per-language keyword + theme wiring | Colour-correct highlighting across ~88 languages |
| 4.6 | User Defined Languages (UDL) | Load N++ UDL XML, preinstalled Markdown UDL, in-app editor modal |
| 5 | Linux (GTK) and macOS (Cocoa) | Same binary builds and runs natively |

---

## Getting Started

For setup instructions on Windows, Linux, and macOS, see [DEVELOPMENT.md](docs/DEVELOPMENT.md).

The short version, once your toolchain is in place:

```sh
git clone --recurse-submodules https://git.fiedler.live/tux/code-plus-plus.git
cd code-plus-plus
cargo build --workspace
cargo run -p codepp-app
```

The canonical repository is hosted on Forgejo at <https://git.fiedler.live/tux/code-plus-plus>. A read-only mirror is pushed to GitHub.

---

## Documentation

- **[DESIGN.md](docs/DESIGN.md)** — full architecture, dependency graph, crate responsibilities, plugin ABI, performance budgets, and the phase plan. Read this if you want to understand any decision in the codebase.
- **[DEVELOPMENT.md](docs/DEVELOPMENT.md)** — platform-by-platform setup for Windows, Linux, and macOS, plus common development tasks and troubleshooting.
- **[CLAUDE.md](CLAUDE.md)** — operational rules and project conventions used by the AI development assistant.

---

## License

Code++ is licensed under the **MIT License**. See [LICENSE](LICENSE) for the full text and [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) for the notices required by upstream components (Scintilla and Lexilla under HPND, plus bundled Rust crates).

The Notepad++ plugin compatibility layer under `plugins/nppcompat-headers/` is an **independent clean-room reimplementation** of the public ABI — message numbers, struct layouts, behavior contracts. No source has been copied from Notepad++; the ABI itself is not copyrightable.

---

## Acknowledgements

Code++ stands on the work of two communities:

- **Notepad++** by Don Ho and contributors — for two decades of proving that a small, fast text editor is worth caring about.
- **Scintilla** and **Lexilla** by Neil Hodgson and contributors — for the editing engine that makes any of this possible.

Code++ is an independent project and is not affiliated with either.
