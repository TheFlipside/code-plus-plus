# Code++: A Notepad++-Style Cross-Platform Editor — Design

**Goal:** a cross-platform, Notepad++-inspired text editor with **Windows-class startup and editing performance**, written in Rust with Scintilla as the editing engine and native UI per platform.

**Compatibility goal:** binary-compatible with existing Notepad++ plugins on Windows; source-compatible (recompile) on Linux and macOS.

---

## 1. Goals and Non-Goals

### 1.1 Goals

- Notepad++-equivalent editing experience: tabs, syntax highlighting, find/replace, find-in-files, encoding control, EOL control, session restore.
- Startup in **tens of milliseconds** on a warm-cache machine.
- Memory profile dominated by buffer text, not framework overhead.
- Native UI on each platform (Win32, GTK, Cocoa) — no Electron, no embedded browser, no GC runtime.
- Plugin host that accepts existing **Notepad++ DLL plugins unchanged** on Windows, and the same plugin source recompiled on Linux/macOS.

### 1.2 Non-Goals

- Reimplementing Scintilla. We use it via FFI; we do not fork or port it.
- LSP, project-wide indexing, Git integration, AI assist — out of scope for v1. Plugins may add them later.
- Web-based or mobile builds.
- A Rust-stable plugin ABI. Plugins speak C.

---

## 2. High-Level Architecture

### 2.1 Component graph

```text
                       app (bin)
                         │
            ┌────────────┼────────────┐
            ▼            ▼            ▼
       ui_win32      ui_gtk       ui_cocoa     (one selected at build time)
            │            │            │
            └────────────┼────────────┘
                         ▼
                       shell
                         │
            ┌────────────┼────────────┐
            ▼            ▼            ▼
         editor       plugin-host   core
            │            │            │
            ▼            ▼            ▼
      scintilla-sys   platform     (std only)
            │
            ▼
       Scintilla + Lexilla (C/C++, vendored)
```

**Direction is strictly downward.** No upward calls, no cycles. Higher layers hold handles to lower layers; lower layers fire events that higher layers subscribe to via channels (never via callbacks into higher crates).

### 2.2 Crate responsibilities

| Crate | Responsibility | Allowed deps |
| --- | --- | --- |
| `core` | Pure data + logic: session model, file I/O, encoding/EOL detection, settings, history bookkeeping. **No UI, no Scintilla, no platform code.** Headless-testable. | `std`, small utility crates only |
| `editor` | Safe Rust wrapper over `scintilla-sys`. Owns nothing OS-specific beyond an opaque `EditorHandle` (newtype per platform under `#[cfg]`). | `scintilla-sys`, `core` (for shared types only — not for state) |
| `scintilla-sys` | **Our own** custom `-sys` crate. Raw FFI to Scintilla 5.x + Lexilla. Vendors the C/C++ source via git submodule and builds it with `cc`. **Not the unmaintained crates.io `scintilla-sys`.** | `cc` (build), `bitflags` |
| `platform` | OS-specific utilities: config paths, dynamic library loading, file watching, process info. | `std`, `windows`, `nix`, etc. behind `#[cfg]` |
| `plugin-host` | Loads N++-compatible plugin DLLs/SOs, owns the NPPM message dispatcher, exposes a strongly-typed Rust event bus to the rest of the app. | `core`, `editor`, `platform` |
| `shell` | Glue layer that owns `Session`, `EditorHandle`s, and the plugin host. Defines the `UiPlatform` trait that UI crates implement. | `core`, `editor`, `plugin-host`, `platform` |
| `ui_win32` | Win32 window, menus, tabs, dialogs, status bar. Implements `UiPlatform`. | `shell`, `windows` crate |
| `ui_gtk` | GTK equivalent. | `shell`, `gtk4` |
| `ui_cocoa` | Cocoa equivalent. | `shell`, `objc2`, `objc2-app-kit` |
| `app` | Thin binary: parses args, picks the UI backend at compile time, calls `shell::run()`. | `shell`, one of `ui_*` |

### 2.3 What moved and why

- The `UiPlatform` trait that the original draft put in `core` lives in `shell`. `core` must be buildable and unit-testable without any window system in scope.
- Plugin loading mechanics (`LoadLibrary`/`dlopen`) live in `platform`. The plugin **registry, lifecycle, and message dispatcher** live in `plugin-host`. The split keeps OS calls out of plugin-host's logic.
- `editor` does **not** depend on `core` for state, only for shared value types (e.g., `Eol`, `Encoding`). This breaks the cycle in the original draft.

---

## 3. Workspace Layout

This is a real Cargo workspace, not a multi-module crate.

```text
code-plus-plus/
├── Cargo.toml                    # [workspace] manifest
├── Cargo.lock
├── DESIGN.md
├── README.md
├── rust-toolchain.toml           # pin a specific stable rustc
├── .github/workflows/ci.yml
├── crates/
│   ├── core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── session.rs
│   │       ├── file.rs
│   │       ├── encoding.rs
│   │       ├── eol.rs
│   │       └── settings.rs
│   ├── scintilla-sys/
│   │   ├── Cargo.toml
│   │   ├── build.rs              # compiles vendored Scintilla + Lexilla
│   │   ├── src/lib.rs            # extern "C" decls, message constants
│   │   └── vendor/
│   │       ├── scintilla/        # git submodule, Scintilla 5.x
│   │       └── lexilla/          # git submodule, Lexilla 5.x
│   ├── editor/
│   ├── platform/
│   ├── plugin-host/
│   ├── shell/
│   ├── ui_win32/
│   ├── ui_gtk/                   # added in Phase 5
│   ├── ui_cocoa/                 # added in Phase 5
│   └── app/
├── plugins/
│   ├── example-hello/            # in-tree sample plugin (DLL)
│   └── nppcompat-headers/        # the C headers a plugin author #includes
└── tools/
    └── npp-plugin-test/          # harness that loads a real N++ plugin
```

Top-level `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = ["crates/*", "plugins/example-hello"]
default-members = ["crates/app"]
```

---

## 4. External Native Dependencies

### 4.1 Scintilla and Lexilla — vendored and built from source

We do **not** depend on the crates.io `scintilla-sys` (last meaningful release is years stale, predates Scintilla 5.x's split into Scintilla + Lexilla). We ship our own.

- **Vendoring:** `crates/scintilla-sys/vendor/scintilla` and `vendor/lexilla` are git submodules pinned to specific Scintilla and Lexilla release tags (initial target: latest stable as of project start; tag pin is bumped deliberately, never floating).
- **Build:** `build.rs` compiles the C/C++ source with the `cc` crate. Per platform:
  - **Windows:** compile `scintilla/win32/*.cxx`, link `user32`, `imm32`, `ole32`, `oleaut32`, `msimg32`. Build static, link static. No `scintilla.dll` shipped.
  - **Linux:** compile `scintilla/gtk/*.cxx`, link against system GTK 4 (`pkg-config gtk4`).
  - **macOS:** compile `scintilla/cocoa/*.mm`, link `Cocoa`, `QuartzCore`.
  - **Lexilla** is built the same way and statically linked into the same crate.
- **Output:** one static archive per target, exposed to `editor` as a single `extern "C"` surface.
- **Why static:** smaller distributable, no DLL-hell, identical loader hot path on every machine, supports the startup-time goal.

### 4.2 Direct-call API (the speed path)

Scintilla exposes two ways to send messages:

1. `SendMessage(hwnd, SCI_..., wparam, lparam)` — works always, costs a window-message round trip.
2. `SCI_GETDIRECTFUNCTION` + `SCI_GETDIRECTPOINTER` — returns a `(fn_ptr, instance_ptr)` pair. Calling the function pointer directly skips the message pump. **This is what gives Notepad++ its keystroke latency.**

`editor` obtains the direct-call pair once when each Scintilla control is created and stores it on the `EditorHandle`. Hot operations (`insert`, `replace`, `set_sel`, `text_length`, `get_text_range`, lexer state queries) go through the direct path. `SendMessage` is reserved for setup, cross-thread one-shots, and operations that must be ordered with the window message queue.

This must be in place from Phase 1 — retrofitting it later means rewriting every `editor` method.

### 4.3 Other native dependencies

- Windows: `windows` crate (Microsoft's official bindings), pinned minor version.
- Linux: `gtk4`, `glib`, `gio` via `gtk4-rs`. Compile-time linked through `pkg-config`.
- macOS: `objc2`, `objc2-app-kit`.
- Cross-platform utilities: `notify` (file watching), `parking_lot` (locks), `crossbeam-channel` (cross-thread messaging), `bitflags`, `tracing` + `tracing-subscriber`, `serde` + `quick-xml` (session.xml).

All version-pinned in workspace `[workspace.dependencies]` and inherited by member crates.

---

## 5. Cross-Cutting Concerns

### 5.1 Encoding detection and preservation

- **BOM-prefixed:** UTF-8, UTF-16 LE, UTF-16 BE, UTF-32 LE/BE — trivial.
- **No BOM:** statistical detection in `core::encoding`. Step order:
  1. Try strict UTF-8 decode of the first 64 KiB. If valid and contains any non-ASCII byte, decide UTF-8.
  2. If pure ASCII, decide UTF-8 (lossless).
  3. UTF-16 without BOM heuristic: count zero bytes in even vs odd positions in the first 8 KiB. Strong skew in either direction → UTF-16 LE/BE.
  4. Fall back to system default codepage (CP1252 on Windows-en, GB18030 on Windows-zh-CN, etc.) via the `encoding_rs` crate.
- **Preservation:** the detected encoding is the default save encoding for that buffer. Changing it is an explicit menu action.
- **Conversion failures on save:** show a dialog with the offending bytes' positions, never silently lose data.

### 5.2 EOL detection and preservation

`core::eol::Eol = { Lf, CrLf, Cr, Mixed }`.

- Detected on first read by counting line endings in the first 64 KiB.
- Preserved on save unless the user explicitly changes it (Edit → EOL Conversion).
- `Mixed` is shown in the status bar with a warning glyph; saving as `Mixed` keeps each line's original ending.

### 5.3 External file change detection

- `platform::watch::Watcher` wraps `notify` and emits events on a `crossbeam_channel`.
- For each open file, `shell` registers a watch; on modification the UI thread asks: "Reload? Keep my version? Diff?" — same UX as Notepad++.
- Watching is per-file, not per-directory, to keep startup cost zero.

### 5.4 Cross-thread UI marshaling

Win32 (and the others) require window-handle operations on the thread that created the window. Workers cannot call `SendMessage` to a UI HWND safely, and they cannot touch Scintilla state directly.

Pattern, mandatory for every worker that produces a UI-visible result:

1. Worker pushes a typed message onto a `crossbeam_channel::Sender<UiTask>` owned by the UI thread.
2. Worker calls `PostMessage(ui_hwnd, WM_APP_WAKE, 0, 0)` (Win32) / `g_main_context_invoke` (GTK) / `dispatch_async` to main queue (Cocoa) to wake the UI thread.
3. UI thread's wake handler drains the channel and applies each `UiTask` (which may call into `editor` on the UI thread, where direct-calls are safe).

`shell` owns the `UiTask` enum and the channel ends. UI crates only know how to wake themselves; they do not invent their own marshaling.

### 5.5 Logging and diagnostics

- `tracing` throughout. Default subscriber is **off** in release builds (zero cost) and **on** when `--verbose` or `CODEPP_LOG=info` is set.
- One log sink: stderr in dev, rotating file in `%LOCALAPPDATA%\code-plus-plus\logs\` in release.
- Plugin host wraps every plugin call in a `tracing::span!` so misbehaving plugins are identifiable.

### 5.6 Testing strategy

| Layer | Test type |
| --- | --- |
| `core` | Unit tests, no FFI, no UI. Encoding detection, EOL detection, session round-trip, settings parse — all pure functions. |
| `scintilla-sys` | Smoke test: link, create Scintilla instance off-screen, send `SCI_INSERTTEXT`, read back via `SCI_GETTEXT`. Catches build/link regressions. |
| `editor` | Integration test: same as above, but exercising the safe wrapper and the direct-call path. |
| `plugin-host` | Loads `plugins/example-hello` and asserts the lifecycle messages fire in order. |
| `tools/npp-plugin-test` | Loads a real, unmodified Notepad++ plugin DLL (e.g. NppExec) and verifies setInfo / getName / getFuncsArray succeed. Only runs on Windows in CI. |
| End-to-end | Manual smoke checklist in `docs/smoke.md` for each phase's demo. |

CI matrix: `windows-latest`, `ubuntu-latest`, `macos-latest`. Linux and macOS are build-only until Phase 5; Windows runs the full test suite from Phase 1.

---

## 6. Plugin System — Notepad++ Compatible from the Ground Up

### 6.1 Compatibility scope

- **Windows binary compatibility:** an existing Notepad++ plugin DLL (compiled against the public Notepad++ plugin headers) loads into Code++ without modification. Code++ exposes the same entry points, the same `NppData` struct, the same `NPPM_*` and `NPPN_*` messages, and the same Scintilla-message forwarding semantics.
- **Linux/macOS source compatibility:** the same plugin source compiles to a `.so`/`.dylib` with the headers we ship in `plugins/nppcompat-headers/`. Binary plugins are inherently platform-specific — there is no cross-platform binary plugin format and we will not invent one.

This is a significant surface. We implement it incrementally (see phases) but the ABI is fixed in Phase 3 and not broken thereafter.

### 6.2 Required plugin entry points

A Code++ plugin DLL must export, exactly as Notepad++ does:

```c
extern "C" __declspec(dllexport) void        setInfo(NppData);
extern "C" __declspec(dllexport) const TCHAR* getName(void);
extern "C" __declspec(dllexport) FuncItem*    getFuncsArray(int* nbF);
extern "C" __declspec(dllexport) void         beNotified(SCNotification*);
extern "C" __declspec(dllexport) LRESULT      messageProc(UINT, WPARAM, LPARAM);
extern "C" __declspec(dllexport) BOOL         isUnicode(void);
```

`NppData` carries the host window handle and the two Scintilla view handles, identical layout:

```c
typedef struct NppData {
    HWND _nppHandle;
    HWND _scintillaMainHandle;
    HWND _scintillaSecondHandle;
} NppData;
```

On Linux/macOS, `HWND` is replaced by an opaque `void*` newtype that maps to the platform's window handle. The header file abstracts this so plugin source stays portable.

### 6.3 Message ABI

Two message families flow through `messageProc`:

- **`NPPM_*`** — host control messages. Initial set covered in Phase 3:
  - File/buffer queries: `NPPM_GETCURRENTSCINTILLA`, `NPPM_GETCURRENTBUFFERID`, `NPPM_GETFULLPATHFROMBUFFERID`, `NPPM_GETBUFFERLANGTYPE`.
  - Editor actions: `NPPM_DOOPEN`, `NPPM_SAVECURRENTFILE`, `NPPM_SWITCHTOFILE`, `NPPM_RELOADFILE`.
  - UI: `NPPM_MENUCOMMAND`, `NPPM_GETMENUHANDLE`, `NPPM_SETSTATUSBAR`.
  - Path/version: `NPPM_GETNPPDIRECTORY`, `NPPM_GETNPPFULLFILEPATH`, `NPPM_GETPLUGINSCONFIGDIR`, `NPPM_GETNPPVERSION` (returns a Code++ version that is range-compatible with Notepad++ for plugin gating).
- **`SCI_*`** — Scintilla messages. Plugins commonly do `SendMessage(scintillaHandle, SCI_INSERTTEXT, ...)`. Because `scintillaHandle` is a real Scintilla `HWND` we created, these just work.

Notifications flow the other way through `beNotified`: `NPPN_READY`, `NPPN_TBMODIFICATION`, `NPPN_FILEOPENED`, `NPPN_FILESAVED`, `NPPN_FILECLOSED`, `NPPN_BUFFERACTIVATED`, `NPPN_LANGCHANGED`, `NPPN_SHUTDOWN`. Phase 3 ships the lifecycle ones; the long tail is filled in during Phase 4.

A coverage matrix lives in `docs/nppm-coverage.md` and is updated every time a new message is implemented. The matrix is the source of truth for plugin compatibility.

### 6.4 Lifecycle

1. App starts → `plugin-host` enumerates `%APPDATA%\code-plus-plus\plugins\*\*.dll` (Windows) / `~/.config/code-plus-plus/plugins/*/*.so` (Linux) / equivalent on macOS.
2. Loading is **deferred**: the host records the file path and registers menu placeholders. No DLL is mapped.
3. First time the user opens the Plugins menu, hovers a plugin entry, or fires a hotkey owned by the plugin, the host loads that DLL: `LoadLibraryW` → resolve six exports → call `setInfo(NppData{...})` → call `getFuncsArray` → install the menu items and shortcuts → fire `NPPN_READY`.
4. On subsequent file events, the host calls `beNotified` on each loaded plugin synchronously on the UI thread (matches N++ semantics).
5. On exit: `NPPN_SHUTDOWN` → unload.

### 6.5 Safety boundaries

- Plugins run in-process, same address space as the editor. Same as Notepad++: a buggy plugin can crash the app. Document this; do not pretend otherwise.
- Plugin calls are wrapped in a `tracing` span and a `catch_unwind` boundary on the Rust side so a panic inside Rust-written plugins doesn't unwind across FFI. C++ plugins that throw past their own ABI are out of scope (they're broken in N++ too).
- Per-plugin timeout for `beNotified`: log a warning if it exceeds 100 ms, but do not kill the plugin. Notepad++ doesn't either; behavior parity matters.

---

## 7. Implementation Phases

### 7.1 The phase rule (read this first)

> **No phase is complete until a fresh `cargo run` of the platform binary demonstrates the phase's behavior end-to-end against the real underlying technology — real Scintilla messages, real Win32/GTK/Cocoa events, real disk I/O, real loaded DLL plugins. Stubs and mocks are acceptable only inside `core` unit tests, never in the integration path.**
>
> The "Demo" column for each phase is the acceptance test. If the demo does not run, the phase is not done — regardless of how much code was written.

This rule exists specifically to prevent the failure mode where layers are built in isolation and the wiring between them is "left for later" until it's no longer feasible.

### 7.2 Phase table

| Phase | Coding work | Demo (must run on a clean machine) |
| --- | --- | --- |
| **0 — Scaffolding** | Create the workspace, all crate skeletons with empty `lib.rs`/`main.rs`. Pin rust-toolchain. Add CI that does `cargo build --workspace` and `cargo test -p core`. Add the Scintilla and Lexilla submodules but **do not yet build them** (build.rs is a no-op). | `cargo build --workspace` green on Windows, Linux, macOS in CI. `cargo run -p app` opens an empty Win32 window with a working menu bar (File → Exit) and closes cleanly. |
| **1 — Scintilla shell** | Wire `scintilla-sys` end-to-end: real `build.rs` compiling vendored Scintilla + Lexilla, real `extern "C"` surface, real link. `editor` obtains direct-call pointers and routes hot ops through them. `ui_win32` creates a real Scintilla `HWND` as a child window and forwards keyboard/mouse messages. | `cargo run -p app` opens a window containing a Scintilla control. Typing produces text. Ctrl+Z/Y undo/redo. Ctrl+A select-all. Right-click context menu (Scintilla's built-in) works. **Exit task:** open Task Manager and confirm process memory under 30 MB with a few KB of typed text. |
| **2 — Core: session, file I/O, encoding** | `core::session` model + `session.xml` round-trip. `core::file::open(path)` reads on a worker thread, posts result to UI thread via the marshaling pattern in §5.4. `core::encoding` and `core::eol` integrated. `platform::watch` wired to detect external changes. | Drag-and-drop a 10 MB UTF-8 file onto the window: it opens without UI freeze (verify with a frame-time log). Status bar shows encoding and EOL. Edit, save, reopen — content and encoding preserved. Close app, reopen — same tab restored at same cursor position via `session.xml`. Modify the file in another editor while Code++ has it open — reload prompt appears. |
| **3 — Multi-tab, menus, plugin host (NPPM v1)** | Tab strip with multiple buffers, each owning a Scintilla view (or one shared view with switched documents — decide based on Scintilla doc-pointer perf testing in this phase). Full menu set: File, Edit, Search, View, Encoding, Language, Settings, Tools, Plugins, Window, ?. `plugin-host` implements the six entry points, lifecycle, and the NPPM/NPPN subset listed in §6.3. Build `plugins/example-hello` as a real DLL in the workspace. | Open three files in three tabs, switch between them. The in-tree `example-hello` plugin DLL is loaded from disk on first menu open and inserts "Hello from plugin" into the active buffer when its menu item is clicked. **Critical second test:** download an unmodified small Notepad++ plugin (e.g., a "convert tabs to spaces" plugin), drop it into the plugins folder, restart, exercise it. It must work. If it does not, the NPPM coverage is incomplete and the phase is not done. |
| **4 — Lexers, search, find-in-files, encoding conversions** | Wire Lexilla: language detection by extension, `SCI_SETILEXER`, style mapping. Find/Replace dialog driving Scintilla's search messages. Find-in-files on a worker pool with results panel. Encoding conversion menu (UTF-8 ↔ UTF-16, ANSI codepages). Long tail of NPPM/NPPN messages so more plugins work. | Open a 5000-line `.cpp` and `.rs` file: syntax highlighting visible, no scroll lag. Find/Replace works in the active buffer. Find-in-files across a 1000-file directory completes in seconds and clicking a result jumps to the line. Convert a UTF-8 file to UTF-16 LE and back; bytes are correct. The NPPM coverage matrix in `docs/nppm-coverage.md` is at least 80% green. |
| **5 — Linux (GTK) and macOS (Cocoa)** | Implement `ui_gtk` and `ui_cocoa` against the same `UiPlatform` trait. Adjust `scintilla-sys` build for the GTK/Cocoa Scintilla backends. Cross-platform plugin loading via `dlopen`/`dlsym`. `app` selects backend at compile time via cargo features (`--features win32` / `--features gtk` / `--features cocoa`), one and only one selected. | `cargo run -p app --features gtk` on Linux: same app, opens, edits, saves, plugin loads (a recompiled `example-hello.so`). Same on macOS with `--features cocoa`. The `core` crate has zero `#[cfg(target_os)]` lines added in this phase — if any appeared, refactor them out before declaring done. |

### 7.3 What gets re-tested at every phase boundary

- `cargo build --workspace` clean on all three OSes (Linux/macOS build-only through Phase 4).
- `cargo test --workspace` green.
- The previous phase's demo still passes.
- Cold-start time measured against the §8 budget. Regression > 20% blocks the phase.

---

## 8. Hard Performance Constraints

These are non-negotiable and verified at each phase boundary.

| Constraint | Budget | Verification |
| --- | --- | --- |
| Cold start (warm cache) to interactive | < 80 ms | Stopwatch from `WinMain` to first paint. Logged when `--perf` flag is set. |
| Single keystroke latency (typed char → Scintilla redraw) | < 5 ms p99 | Frame-time log on a 10k-line file. |
| Open 10 MB UTF-8 file | UI never blocks | Worker thread reads; marshal posts buffer to UI thread incrementally if needed. |
| Memory floor (one empty buffer) | < 25 MB RSS | Read from process info at startup. |
| Undo history | Bounded, default 1000 ops, configurable | Scintilla `SCI_SETUNDOCOLLECTION` + periodic trim. |
| Plugin load on startup | Zero plugins loaded until first user interaction touches them | Trace log at startup shows zero `LoadLibrary` calls until menu open. |
| File I/O thread | Dedicated worker pool, never UI thread | Lints fail if `core::file` is called synchronously from a UI crate. |

If a future feature breaks any of these, it must either be feature-gated (off by default) or be redesigned. No exceptions for "minor" regressions.

---

## 9. Build, Packaging, CI

### 9.1 Build

- `cargo build -p app` produces `code++.exe` (Windows) / `code++` (Linux/macOS).
- All native code (Scintilla, Lexilla) is statically linked. The binary depends only on system libraries (`user32`/`kernel32` on Windows, `libgtk-4` on Linux, `Cocoa.framework` on macOS).
- Release profile: `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, `strip = "symbols"` (debug symbols extracted to a side file for crash decoding).

### 9.2 Distribution

- Windows: a single `.exe` plus a `plugins/` folder. Optional MSI installer in a later phase.
- Linux: tarball + AppImage. `.deb`/`.rpm` later.
- macOS: `.app` bundle with code signing in a later phase.

### 9.3 CI

- **Hosting:** the canonical repository is on Forgejo at <https://git.fiedler.live/tux/code-plus-plus>. A read-only mirror is pushed to GitHub. CI runs on Forgejo Actions only; the GitHub mirror has no workflow.
- **Runners:** three self-hosted Forgejo runners with labels `windows`, `linux`, and `macos`. The build job's matrix maps `runs-on: ${{ matrix.runner }}` directly to those labels. Lint and `cargo-deny` jobs run on the `linux` runner.
- **Required jobs:** `cargo build --workspace --all-targets` on each runner, `cargo test -p codepp-core` on each runner, `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo deny check`.
- **Windows job** additionally runs the Notepad++ plugin compatibility smoke test from Phase 3 onward.
- **Permissions:** workflows declare `permissions: contents: read` at the workflow level.
- **Hardening for self-hosted runners** (these are deliberate decisions enforced from Phase 0 onward, not deferrable):
  - `actions/checkout` is pinned to a commit SHA, never a floating tag. A tag-move supply-chain attack on persistent self-hosted hardware has durable impact (host-disk access, credential exfiltration, pivot to the local network), so SHA-pinning is mandatory. The bump procedure (`git ls-remote ... | update SHA + comment`) lives next to the pin in the workflow file.
  - `actions/cache` is **not used.** Persistent on-disk `target/` and `~/.cargo` provide the natural caching; the cache action's `restore-keys` prefix fallback is a cross-PR poisoning vector.
  - `cargo-deny` is installed via `cargo install --locked cargo-deny`, not via `EmbarkStudios/cargo-deny-action`. Removes the dependency on the Forgejo instance's external-action proxy and one more SHA to track.
- **Phase boundary:** tag the commit (`phase-0-complete`, `phase-1-complete`, ...) only after CI is green on all three runners and the phase demo is recorded.

---

## 10. Open Decisions Deferred

These are explicitly out of scope for v1 and will be revisited only after Phase 5 ships:

- LSP client. Belongs in a plugin.
- Tree-sitter highlighting. Lexilla covers v1; tree-sitter is a later experiment.
- Settings UI beyond an editable `config.xml`. v1 reuses Notepad++'s text-based config style.
- Multi-window (more than one top-level window per process). v1 is single-window, multi-tab.
- Auto-update. Manual download for v1.

---

## 11. Next Action

Proceed to **Phase 0**: create the workspace, the crate skeletons, and the no-op `build.rs` that confirms the Scintilla submodule is present. CI must be green before any Phase 1 work begins.
