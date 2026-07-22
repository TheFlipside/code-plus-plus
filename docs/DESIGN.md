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
| `ui_gtk` | GTK equivalent. | `shell`, `gtk` (gtk3-rs) |
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
├── README.md
├── CLAUDE.md                     # operational rules for AI assist
├── rust-toolchain.toml           # pin a specific stable rustc
├── .github/workflows/ci.yml
├── docs/
│   ├── DESIGN.md                 # this file
│   ├── DEVELOPMENT.md            # platform-by-platform setup
│   └── nppm-coverage.md          # plugin-ABI coverage matrix
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
│   ├── cppmimetools/             # Phase 4 m7: clean-room mimeTools (DLL)
│   ├── cppconverter/             # Phase 4 m7: clean-room NppConverter (DLL)
│   ├── cppexport/                # Phase 4 m7: clean-room NppExport (DLL)
│   └── nppcompat-headers/        # the C headers a plugin author #includes
└── tools/
    └── npp-plugin-test/          # harness that loads a real N++ plugin
```

Top-level `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = [
    "crates/*",
    "plugins/example-hello",
    "plugins/cppmimetools",        # added Phase 4 m7
    "plugins/cppconverter",        # added Phase 4 m7
    "plugins/cppexport",           # added Phase 4 m7
]
default-members = ["crates/app"]
```

---

## 4. External Native Dependencies

### 4.1 Scintilla and Lexilla — vendored and built from source

We do **not** depend on the crates.io `scintilla-sys` (last meaningful release is years stale, predates Scintilla 5.x's split into Scintilla + Lexilla). We ship our own.

- **Vendoring:** `crates/scintilla-sys/vendor/scintilla` and `vendor/lexilla` are git submodules pinned to specific Scintilla and Lexilla release tags (initial target: latest stable as of project start; tag pin is bumped deliberately, never floating).
- **Build:** `build.rs` compiles the C/C++ source with the `cc` crate. Per platform:
  - **Windows:** compile `scintilla/win32/*.cxx`, link `user32`, `imm32`, `ole32`, `oleaut32`, `msimg32`. Build static, link static. No `scintilla.dll` shipped.
  - **Linux:** compile `scintilla/gtk/*.cxx`, link against system GTK **3** (`pkg-config gtk+-3.0`). GTK 3 rather than GTK 4 is forced by upstream: Scintilla ships no GTK 4 backend. `vendor/scintilla/doc/ScintillaDoc.html` documents "GTK 2.24 and 3.x" only, the highest version guard in `gtk/` is `GTK_CHECK_VERSION(3,22,0)`, and the source uses APIs GTK 4 removed outright (`GdkWindow`, `gtk_widget_get_window`, `gtk_container_add`, `gtk_widget_set_events`, `gdk_window_get_origin`, `gtk_style_context_get_background_color`). Targeting GTK 4 would mean porting Scintilla's platform layer, which §1.2 rules out. GTK 3.24 is the final, API-frozen GTK 3 series, so this is a stable target — the same choice Geany and SciTE make. Also compile `gtk/scintilla-marshal.c` (plain C, so it needs its own `cc::Build`).
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
- Linux: `gtk`, `glib`, `gio` via **gtk3-rs** (`gtk` 0.18). Compile-time linked through `pkg-config`. The gtk3-rs crates are archived upstream, so `deny.toml` carries documented `[advisories] ignore` entries for them; GTK 3.24 is itself API-frozen, so there is nothing left for the bindings to track. See §4.1 for why GTK 3 and not GTK 4.
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
- One log sink: stderr in dev, rotating file in `%LOCALAPPDATA%\Code++\logs\` in release.
- Plugin host wraps every plugin call in a `tracing::span!` so misbehaving plugins are identifiable.

### 5.6 Testing strategy

| Layer | Test type |
| --- | --- |
| `core` | Unit tests, no FFI, no UI. Encoding detection, EOL detection, session round-trip, settings parse — all pure functions. |
| `scintilla-sys` | Smoke test: link, create Scintilla instance off-screen, send `SCI_INSERTTEXT`, read back via `SCI_GETTEXT`. Catches build/link regressions. |
| `editor` | Integration test: same as above, but exercising the safe wrapper and the direct-call path. |
| `plugin-host` | Loads `plugins/example-hello` and asserts the lifecycle messages fire in order. From Phase 4 m7 onward, also loads each preinstalled plugin (`cppmimetools`, `cppconverter`, `cppexport`) and asserts at least one NPPM round-trip per plugin (e.g. cppmimetools's base64 round-trip, cppconverter's hex→ASCII selection conversion, cppexport's HTML output containing the lexer-styled spans). |
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

1. App starts → `plugin-host` enumerates `%APPDATA%\Code++\plugins\*\*.dll` (Windows) / `~/.config/Code++/plugins/*/*.so` (Linux) / equivalent on macOS.
2. Loading is **deferred**: the host records the file path and registers menu placeholders. No DLL is mapped.
3. First time the user opens the Plugins menu, hovers a plugin entry, or fires a hotkey owned by the plugin, the host loads that DLL: `LoadLibraryW` → resolve six exports → call `setInfo(NppData{...})` → call `getFuncsArray` → install the menu items and shortcuts → fire `NPPN_READY`.
4. On subsequent file events, the host calls `beNotified` on each loaded plugin synchronously on the UI thread (matches N++ semantics).
5. On exit: `NPPN_SHUTDOWN` → unload.

### 6.5 Safety boundaries

- Plugins run in-process, same address space as the editor. Same as Notepad++: a buggy plugin can crash the app. Document this; do not pretend otherwise.
- Plugin calls are wrapped in a `tracing` span and a `catch_unwind` boundary on the Rust side so a panic inside Rust-written plugins doesn't unwind across FFI. C++ plugins that throw past their own ABI are out of scope (they're broken in N++ too).
- Per-plugin timeout for `beNotified`: log a warning if it exceeds 100 ms, but do not kill the plugin. Notepad++ doesn't either; behavior parity matters.

### 6.6 Preinstalled plugins — clean-room in-tree reimplementations

Notepad++ defaults ship with three preinstalled plugins — **mimeTools**, **NppConverter**, and **NppExport** — and Code++ wants the same out-of-the-box experience. Three in-tree plugin crates (`plugins/cppmimetools/`, `plugins/cppconverter/`, `plugins/cppexport/`) deliver that, built as `cdylib`s against Code++'s own N++-compatible plugin ABI. Beyond the user-facing default-set parity, three real plugins exercising the host's NPPM surface in three different ways are a far stronger ABI smoke test than `example-hello` alone — more dogfood, tighter feedback loop on host bugs.

That they happen to be in-tree clean-room reimplementations rather than bundled upstream binaries is also a licensing constraint, not just a preference: two of the three upstream plugins (`mimeTools`, `NppConverter`) are licensed **GPLv3**; the third (`NppExport`) ships without a license file, which under default copyright is "all rights reserved". Bundling either category inside Code++'s MIT-licensed release archive is a hard no:

- **GPLv3 plugins:** the plugin and the host are independent works at runtime — a user who downloads `mimeTools.dll` separately and drops it into the plugins directory is fine, the same way it works in N++ (no special arrangement required). The problem is **redistribution**: shipping a GPLv3 binary inside Code++'s release archive makes Code++ the distributor of a GPL-licensed work, and GPLv3's terms then govern what the distributor can do — terms that conflict with the MIT license under which Code++ as a whole is released. This mirrors the workspace's own `deny.toml` policy, which denies copyleft licenses on Cargo dependencies for the same redistribution reason.
- **Unlicensed plugins:** simply not legally redistributable.

So the decision: clean-room reimplement all three under MIT inside the Code++ workspace. The functionality is purely buffer-text transformation (encode/decode, hex/ASCII conversion, syntax-aware export) so the reimplementations are tractable. Users who want the upstream GPLv3 plugins can still install them by hand — runtime loading by the plugin host is the same as for any other third-party plugin and doesn't trigger any redistribution obligation on Code++.

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
| **3 — Multi-tab, menus, plugin host (NPPM v1)** | Tab strip with multiple buffers, each owning a Scintilla view (or one shared view with switched documents — decide based on Scintilla doc-pointer perf testing in this phase). Full menu set: File, Edit, Search, View, Encoding, Language, Settings, Tools, Macro, Run, Plugins, Window, ?. `plugin-host` implements the six entry points, lifecycle, and the NPPM/NPPN subset listed in §6.3. Build `plugins/example-hello` as a real DLL in the workspace. | Open three files in three tabs, switch between them. The in-tree `example-hello` plugin DLL is loaded from disk on first menu open and inserts "Hello from plugin" into the active buffer when its menu item is clicked. **Critical second test:** download an unmodified small Notepad++ plugin (e.g., a "convert tabs to spaces" plugin), drop it into the plugins folder, restart, exercise it. It must work. If it does not, the NPPM coverage is incomplete and the phase is not done. |
| **4 — Lexers, search, find-in-files, encoding conversions, preinstalled plugins** | Wire Lexilla: language detection by extension, `SCI_SETILEXER`, style mapping. Find/Replace dialog driving Scintilla's search messages. Find-in-files on a worker pool with results panel. Encoding conversion menu (UTF-8 ↔ UTF-16, ANSI codepages). Long tail of NPPM/NPPN messages so more plugins work. **Three in-tree preinstalled plugins** (`cppmimetools`, `cppconverter`, `cppexport`) — clean-room MIT-licensed reimplementations of the canonical Notepad++ defaults (see §6.6 for the rationale). Built as `cdylib`s against our own N++-compatible plugin ABI; ship with the Windows release artifact from this phase, extended to Linux/macOS in Phase 5 alongside the rest of the cross-platform bring-up. **m8 — File menu wiring:** flesh out the `MF_GRAYED` placeholders in `build_main_menu` — File→New, Open, Save As, Save All, Close All, Reload — using `GetOpenFileNameW`/`GetSaveFileNameW` on Win32 (cross-platform abstractions land in Phase 5 with the rest of the dialog primitives). Only `Shell::open_file`, `save_current_to_disk`, `close_active_tab`, and `confirm_reload` exist today (`Shell::new` aside) from m1–m3; m8 adds the missing siblings (`new_untitled`, `save_buffer_as`, `save_all`, `close_all`, `reload_active`) and wires every one to its menu ID. **File→Print stays greyed in m8** — printing needs `PrintDlgExW` + a GDI render pass that styles via Scintilla's `SCI_FORMATRANGEFULL`, which is a milestone of its own and a natural fit for the Phase 5 cross-platform dialog/print abstraction. | Open a 5000-line `.cpp` and `.rs` file: syntax highlighting visible, no scroll lag. Find/Replace works in the active buffer. Find-in-files across a 1000-file directory completes in seconds and clicking a result jumps to the line. Convert a UTF-8 file to UTF-16 LE and back; bytes are correct. On the Phase 4 Windows runner, all three preinstalled plugins load on first menu open and their items work end-to-end (cppmimetools round-trips base64; cppconverter converts a hex selection to ASCII; cppexport writes the active buffer to HTML with the active lexer's styling). The NPPM coverage matrix in `docs/nppm-coverage.md` is at least 80% green. **m8 demo:** every File-menu entry except Print is enabled and works — `Ctrl+N` opens an untitled buffer, `Ctrl+O` opens a file via the OS open dialog, `Ctrl+Shift+S` writes to a chosen path, Save All persists every dirty tab, Close All closes the workspace (with the existing dirty-prompt path), Reload re-reads the active file from disk (with the same prompt as the file-watcher path). Print remains greyed and is tracked for Phase 5. |
| **4.5 — Lexer host-side wiring (keywords + per-language theming)** | Phase 4 m1 stood up the lexer plumbing (Lexilla statically linked, `SCI_SETILEXER` issued on lang change, `SCI_COLOURISE` triggers a re-style on language switch, `tab.lang` persists in `session.xml`), but the inline `if lang == L_C` chain in `Win32Ui::apply_lang` only configures **C, C++, and Rust** with explicit keyword lists and per-style colour themes. Every other entry in `core::lang::LANG_TABLE` (Python, JSON, JavaScript, Bash, Lua, SQL, YAML, TOML, CSS, HTML, … ~80 more) maps to a Lexilla lexer that **is statically linked** and tokenises correctly, but the host doesn't pass keyword lists or set per-style fore/back colours — so comments / strings / numbers / keywords all resolve to `STYLE_DEFAULT` after `SCI_STYLECLEARALL` and render uniformly black-on-white. The lexer is doing its job; the host just isn't translating the lexer's classifications into visible colours. **Phase 4.5 closes that gap.** Replace the inline branches with a table-driven framework (`fn lang_theme(LangType) -> Option<&'static LangTheme>`) where each row carries the keyword classes and a `&[(SCE_*_INDEX, StyleSlot)]` mapping into a shared palette. Migrate C / C++ / Rust onto the new structure as a no-op verification, then expand row-by-row across `LANG_TABLE`. The starter SCE_* constants for ~10 popular lexers (Python, JSON, Bash, Lua, SQL, YAML, TOML, CSS) already live in `crates/scintilla-sys/src/lib.rs`; new lexers add SCE_* batches as they're wired. **Tracking:** `docs/lexers-coverage.md` is the per-language progress matrix — same shape as `docs/nppm-coverage.md`. Each row marks keywords/theme status (✅ wired / 🟡 lexer attached without theme / ⚫ no Lexilla lexer at all — `LANG_TABLE` row's `lexer` is `None`, currently only `Normal Text`). The matrix gates the phase at ≥80% ✅ — same convention Phase 4 uses for `nppm-coverage.md`. The remaining 🟡 rows past that bar get formally tracked for follow-on commits; `Normal Text` (⚫ by design) is excluded from the percentage. | Open a sample file in each implemented language and confirm comments, strings, numbers, and keywords pick up distinct visible colours — best-effort highlighting matches Notepad++ defaults. Pick "Python" from the Language menu on a `.foo` file with `if`/`def`/`return` keywords: those tokens render bold-blue. The `lexers-coverage.md` matrix is at least 80% ✅ to declare phase 4.5 ready, with the residual ⚫/🟡 rows tracked for follow-on commits or formally accepted as `lexer: None` parking. **Performance gate (DESIGN.md §8) still applies:** a tab switch must remain under the keystroke-latency budget — table-driven theme application is one `style_set_fore` per entry (~20 calls), well inside budget. |
| **4.6 — User Defined Languages (UDL)** | Adds first-class support for Notepad++-style User Defined Languages (UDL) — the XML-driven custom-lexer format that lets users describe their own syntax highlighting for languages Scintilla / Lexilla doesn't ship a lexer for. N++'s UDL is fundamentally a Notepad++ layer over Scintilla (not a Lexilla lexer), so Code++ builds its own UDL runtime on `SCLEX_CONTAINER` (Scintilla's "host-side styling" mode) driven by `SCN_STYLENEEDED` notifications. Three milestones: **m1 — Loading + runtime.** New `crates/udl` crate parses UDL XML via `quick_xml`, produces one `UdlDefinition` per file. Startup scan of `<config_dir>/userDefineLangs/*.udl.xml` registers every loaded definition in `LANG_TABLE` alongside the built-in Lexilla-backed lexers, extending `LangType` with a dynamic-id space so UDL-language IDs live above the built-in constant range. `SCI_SETLEXER(SCLEX_CONTAINER)` replaces `SCI_SETILEXER` for UDL buffers; the `SCN_STYLENEEDED` handler drives a tokeniser walk against the loaded rules. Bundles [Edditoria's markdown-plus-plus](https://github.com/Edditoria/markdown-plus-plus) (MIT) as preinstalled UDLs — copied into `assets/preinstalled-udls/` at build time (**not** a submodule; only two files, changes rare, submodule tooling overhead isn't justified). Provenance notice + upstream `LICENSE` copy kept alongside per the same discipline as `plugins/nppcompat-headers/`. **m2 — Language menu polish.** "User-Defined language" submenu at the bottom of the Language menu (below the last built-in lexer, currently YAML, after a separator). Submenu contains "Define your language…" (menu ID reserved but greyed until m3 lands), "Open User Defined Language folder…" (opens the OS file explorer at `<config_dir>/userDefineLangs/`), and "Notepad++ User Defined Languages Collection" (opens `https://github.com/notepad-plus-plus/userDefinedLanguages` in the default browser). Loaded UDLs themselves live *flat at the top level* of the Language menu (below the "User-Defined language" submenu entry, after a separator), matching N++'s layout so a click on a loaded UDL is one hover-and-click rather than two — each with a menu ID that switches the active buffer's language via the same path built-in-lexer menu items already take. **m3 — The UDL editor modal.** Four-tab modeless dialog (Folder & Default / Keywords Lists / Comment & Number / Operators & Delimiters) with a nested Styler dialog (font family, size, bold/italic/underline, foreground/background colours + transparency, per-slot nesting checkboxes). Round-trips against the same XML format `crates/udl` consumes so hand-authored UDLs and modal-authored UDLs are interchangeable. Modeless-per-N++ semantics: the modal stays open while the user edits, and the active buffer restyles live on every property change (`SCI_COLOURISE(0, -1)` after each mutation is well inside the keystroke budget for a 5000-line file). **Phase 5 polish item** (deferred, tracked in §7.4): dialog migration to standard `#32770` dialog templates — the UDL editor is a natural candidate to land alongside Find/Replace and Goto since it also lives on custom `WS_POPUP` glue. | Drop an unmodified Notepad++ UDL XML into `<config_dir>/userDefineLangs/` (e.g. any file from the Notepad++ collection linked above), restart Code++, verify it appears at the bottom of the Language menu (flat, below the "User-Defined language" submenu entry), apply it to a matching-extension file, and confirm the highlighting matches Notepad++'s rendering of the same file byte-for-byte. Open a `.md` file, choose "Markdown (preinstalled)" from the bottom of the Language menu, confirm headers / links / bold / italic / code blocks all highlight per Edditoria's palette. **m3 demo:** open "Define your language…", create a new UDL from scratch (name, extension, one keyword class, one delimiter, one comment shape), save, apply to a buffer, close/reopen the app, confirm the saved UDL loads on next start and its rules still apply to the buffer. Performance: a tab switch onto a UDL buffer stays inside the keystroke-latency budget (§8) — the container-lexer tokeniser only paints the requested `SCN_STYLENEEDED` range, not the whole file, so it operates in the same envelope as Lexilla lexers. |
| **5 — Linux (GTK 3) and macOS (Cocoa)** | Implement `ui_gtk` and `ui_cocoa` against the same `UiPlatform` trait. Adjust `scintilla-sys` build for the GTK/Cocoa Scintilla backends. **m1 — Linux foundation (landed).** `scintilla-sys` and `editor` un-gated off `#![cfg(target_os = "windows")]` (neither contained platform code beyond its FFI entry points); `build.rs` grew `build_scintilla_gtk` compiling the vendored GTK 3 backend plus Lexilla, with the shared `src/*.cxx` list factored into `scintilla_core_sources`; new `crates/ui_gtk` opens a real window hosting a real Scintilla widget with the §4.2 direct-call pair captured via `EditorHandle::from_gtk_widget`; `app` selects the backend by cargo feature. **m2 — Shell wiring (landed).** m2a lifted the Phase 4.5 lexer theme table (`StyleSlot`, `LangTheme`, 85 `*_THEME` rows and ~200 supporting tables, `lang_theme`, `extra_fold_properties` and the style helpers) out of `ui_win32` into `codepp_editor::theme`, so both backends share one table instead of duplicating ~8000 lines; `ui_win32::apply_lang` reduced to a UDL check plus `apply_lang_theme`. Its 88 tests now run on every platform rather than only Windows. m2b wired `Shell` into `ui_gtk`: a `GtkUiState`/`split()` pair mirroring `WindowState`, the §5.4 wake via `glib::MainContext::invoke`, the 34 Linux-side `UiPlatform` methods, a 7-part status bar matching the Win32 layout, a File menu on `GtkFileChooserNative`, session restore and the 7-second auto-save. **`crates/shell` needed no un-gating** — every API m2 uses already sat outside the Windows gates, which cluster on the plugin host and the Find/Replace drivers. m2 stops short of the tab strip, the dialogs, the toolbar, UDL styling and the plugin host (`platform::dynlib` still has no `dlopen` arm). Cross-platform plugin loading via `dlopen`/`dlsym`. `app` selects backend at compile time via cargo features (`--features win32` / `--features gtk` / `--features cocoa`), one and only one selected. | `cargo run -p app --features gtk` on Linux: same app, opens, edits, saves, plugin loads (a recompiled `example-hello.so`). Same on macOS with `--features cocoa`. The `core` crate has zero `#[cfg(target_os)]` lines added in this phase — if any appeared, refactor them out before declaring done. |

### 7.3 What gets re-tested at every phase boundary

- `cargo build --workspace` clean on all three OSes (Linux/macOS build-only through Phase 4).
- `cargo test --workspace` green.
- The previous phase's demo still passes.
- Cold-start time measured against the §8 budget. Regression > 20% blocks the phase.

### 7.4 Phase 5 polish items deferred from earlier phases

These are not blockers for the phase that surfaced them, but get addressed in Phase 5 as part of the cross-platform bring-up.

- **Migrate `ui_win32` modal/modeless dialogs from custom `WS_POPUP` classes to standard `#32770` dialog templates.** The Goto and Find/Replace dialogs currently use `CreateWindowExW` with our own registered class. Win11 paints the client area of `WS_POPUP | WS_CAPTION` windows through DWM/UxTheme, outside the `WM_ERASEBKGND` message path entirely — so `WNDCLASSEX.hbrBackground` is silently overridden and our chrome ends up at a slightly different shade than what the system paints. The standard dialog class (`#32770`, instantiated via `DialogBoxIndirectParamW` / `CreateDialogIndirectParamW` with a `DLGTEMPLATEEX` in memory) is the only window class Microsoft retrofitted with cooperative themed-background logic; that's the path Notepad++ uses, and it's why N++'s dialog chrome blends seamlessly. Migrating means a different dispatch model (dialog procs return `BOOL`, use `EndDialog` instead of `DestroyWindow`, etc.) and constructing in-memory `DLGTEMPLATE` byte streams — meaningful work but mechanical. Worth doing in Phase 5 alongside the `ui_gtk`/`ui_cocoa` brings-up because Linux/macOS will need their own dialog primitives anyway, so this is a natural moment to redesign the cross-platform dialog abstraction.

- **~~Replace in Files: handle open files like Notepad++ does.~~ Done for clean tabs (Phase 5); dirty tabs still declined.** The orchestrator used to skip every file the user had open, so a Replace in Files silently did nothing to them. It now hands the replacement it computed to `Shell::drain`, which applies it to the tab's Scintilla buffer — the user keeps undo, the file watcher stays silent, and nothing is written underneath them. Living in `drain` rather than in each backend's results dock means both platforms get it with no UI wiring, and it is testable without one: the end-to-end test runs a real worker pool over real files and asserts the closed file is rewritten on disk while the open one is not and its buffer receives the edit instead.

  **A tab with unsaved edits is still declined**, and that is deliberate rather than unfinished. The worker computes its replacement from the bytes *on disk*, which are not what the user is looking at in a dirty buffer, so applying it would discard their work. Closing that case means re-searching the buffer itself, and the two searches do not use the same engine: find-in-files compiles to the `regex` crate because it runs off the UI thread, while an in-editor search goes through Scintilla's C++11 engine, and the dialects disagree on lookarounds, backreferences and more. Re-searching would risk replacing a different set of matches than the results dock just reported for that file. Resolving it properly means either accepting that divergence explicitly, or moving find-in-files onto Scintilla's engine — which would mean running searches on the UI thread, against DESIGN.md §5.4. Worth deciding rather than drifting into.

  Also outstanding, and small: the counts are reported through `FifEvent::ReplacedInOpenBuffer` but no backend surfaces them yet. They belong on the completion toast alongside the existing "scanned N files" summary.

- **Synchronous plugin notification delivery for `NPPN_FILEBEFORECLOSE` (and friends).** Code++ pushes notifications onto `Shell.pending_notifications` and the UI delivers them to plugins after the current `&mut Shell` borrow ends. That model is right for most events (FILECLOSED, BUFFERACTIVATED, LANGCHANGED, …), but Notepad++ delivers `NPPN_FILEBEFORECLOSE` *synchronously* while the closing buffer is still in its data structures so a plugin's `beNotified` handler can call back into `NPPM_GETFULLPATHFROMBUFFERID(id)` and get the path. In Code++ the tab is removed from `Shell.tabs` before the queued notification fires, so that callback returns `-1`. Same applies to a future `NPPN_FILEBEFOREOPEN`. The fix is a synchronous-dispatch hook on `Shell` (a callback registered by the UI at startup that dispatches a single notification through `plugin-host::notify_all` mid-operation), used only for the BEFORE-* family. The deferred queue stays for the rest. `NPPM_RELOADBUFFERID`'s `with_alert == true` path needs the same mechanism — to synthesise a `PendingDialog::ConfirmReload` from inside `dispatch_plugin_message` so the user gets the same prompt the file-watcher path uses. Both gaps are documented in `docs/nppm-coverage.md` and tracked here so they land together with the rest of the plugin-host sync work in Phase 5.

- **`NPPM_SETBUFFERFORMAT` should issue `SCI_CONVERTEOLS` on the addressed buffer.** Today the dispatcher's `set_buffer_format` only flips `tab.eol` metadata — the Scintilla document's existing line-ending bytes are not rewritten. N++ additionally calls `SCI_CONVERTEOLS(SC_EOL_*)` on the buffer to normalise every line ending in place, so a plugin doing "set the EOL then save" produces a file with consistent line endings throughout. Code++'s metadata-only mutation is acceptable for empty buffers and for the immediate-after-load case (where there's nothing to convert yet) but not for general use. The fix needs UI-side cooperation: a `UiPlatform` method that takes a Scintilla doc pointer (from the addressed `Tab`) and the target EOL, swaps the editor's bound document to that pointer, sends `SCI_CONVERTEOLS`, and restores the original — the same doc-pointer-swap dance as Polish D's in-buffer FIF replace. Worth landing alongside the synchronous-notification work above since both require `Shell` to call back into the UI mid-dispatch.

- **~~`EditorHandle` has no lifetime tie to the Scintilla widget it points at — decide the ownership model before tabs land on GTK.~~ Decided (Phase 5 m3): single view.** The question was real — `EditorHandle` is `Copy` with no `Drop` and holds raw pointers into a widget without expressing a lifetime, so nothing in the type system stops a copy outliving what it points at. `ui_gtk` resolves it the way `ui_win32` already did: one Scintilla widget is created at startup and never destroyed, removed or reassigned, and tabs get their own buffers through `SCI_SETDOCPOINTER` rather than their own views. The dangling-`direct_ptr` case therefore cannot arise, because there is no path that finalises a view.

  Recorded in three places rather than one, because a decision that lives only in a design document is not enforced: `EditorHandle::from_gtk_widget`'s safety section now states the *continuing* obligation (the original wording covered only the call, not the handle's later use), `GtkUiState.sci_widget`'s doc states how this backend discharges it, and `ui_gtk` carries a source-level guard asserting exactly one `scintilla_new()` with no destroy or reassignment. The guard is source-level on purpose: destroying the view would fault inside vendored C++ on the next direct call rather than fail an assertion, so a runtime test cannot observe the failure this exists to prevent. Mutation-verified against a second view, a destroy, and a reassignment.

  **This constrains any future backend.** A `ui_cocoa` that gives each tab its own `NSView` inherits the original problem and would have to tie the handle's lifetime to the widget instead.

- **Find-in-Files symlink hardening is Unix-only; Windows needs `FILE_FLAG_OPEN_REPARSE_POINT`.** The FIF worker opens candidate files with `O_NOFOLLOW | O_NONBLOCK` and then re-validates the file type from `fstat` on the open descriptor, so a path swapped for a symlink or a FIFO between the walker's `file_type()` check and the worker's open is refused rather than followed (`crates/shell/src/fif.rs::read_capped`). Both flags are Unix-only. On Windows the post-open type check still rejects a directory or a device, but a reparse point is followed — yielding content from outside the search root attributed to the wrong path, and, if it resolves somewhere slow such as an unreachable UNC host, a block inside `CreateFileW` that the type check never gets to veto. That hang is the same risk class `MAX_ACTIVE_JOBS` already budgets for, so it is bounded rather than open-ended. Closing it means passing `FILE_FLAG_OPEN_REPARSE_POINT` via `std::os::windows::fs::OpenOptionsExt::custom_flags` and rejecting `is_symlink()` alongside the existing `is_file()` check — mechanical, but it needs a Windows runner to verify, and the three regression tests covering this on Unix (`read_capped_refuses_a_fifo`, `read_capped_refuses_a_symlink`, `atomic_write_cannot_be_redirected_by_a_planted_temp_symlink`) are `#[cfg(unix)]` because their *setup* needs `mkfifo`/`symlink`. Windows CI therefore has no adversarial coverage of this path today; the sibling tests should land with the fix. The same commit should scope one further claim: `atomic_write`'s comment states that rename replaces a symlink at the destination rather than following it, which is verified POSIX behaviour but is asserted unconditionally even though `tempfile::NamedTempFile::persist` goes through `MoveFileExW` on Windows, where it is untested.

- **Decide, explicitly, whether the 43% memory rise from m1 is accepted.** §8 now carries the per-mapping attribution rather than a guess: PSS went 8.17 MB (m1) to 11.81 MB (m3), and the growth is **+2.60 MB of `.text`, +0.85 MB of `.rodata`, and exactly zero heap**. So it is static footprint — code and tables that are resident because startup executes them — not a leak, and it does not scale with the number or size of open files. That is the strongest case for accepting it, and 11.8 MB is 47% of the 25 MB budget. It is still past §8's own ">20%" bar, and §8 says non-negotiable with no headroom clause, so the rule wants an explicit answer rather than a silent pass. If the answer is "reduce it instead", the three untried levers are named in §8 — compile `lang_theme`'s 85-branch chain to a table lookup, `opt-level = "s"`, or `lto = "fat"` — and each is a measurable experiment, not a guess. Whoever decides should also fix the baseline: §8's m1 row is the *only* figure the doc ever recorded before m3, which is how a two-milestone drift went unnoticed; recording PSS at every phase boundary would have caught it at m2.

- **Find what stalls ~1% of keystrokes above roughly 15 characters per second.** §8 now characterises the tail properly — 5 016 samples at a fast-human rate put p99 at 4.62 ms, inside the 5 ms budget — but at 20 char/s p99 jumps to 65 ms while p50 and p90 barely move. That shape is a cliff, not a backlog: something occasional blocks for tens of milliseconds rather than everything getting slower. Two candidates were tested and refuted, so do not start there: the 7-second session auto-save (interval raised to 100 000 s, p99 unchanged at 65.4 ms) and Lexilla restyling (same-size plain-text file with no lexer, p99 unchanged at 60.5 ms). It is content-independent and rate-dependent, and the worst sample at human rates was 17.8 ms, so it never reaches the budget in normal use.

  Next step is a profiler, not another black-box run — the remaining suspects (GTK frame-clock scheduling, compositor frame misses, a status-bar label width change forcing a relayout, GDK event coalescing) are indistinguishable from the outside. `--perf` now reports the arrival distribution alongside latency, which is what any repeat of this needs: the input rate has to be measured rather than assumed, and it must be checked against the numbers in §8 rather than against a tool's `--delay` flag.

- **`NPPM_SETSTATUSBAR` bypasses the display-sanitization consolidation.** The dispatcher reads the plugin's wide-pointer, hands the string to `Shell::set_status_bar`, and both backends pipe it straight to the status bar — `ui_win32::set_plugin_status` calls `write_status_part`, `ui_gtk::set_plugin_status` calls `label.set_text` — with no call to `sanitize_display_char` / `sanitize_filename_for_display`. `write_status_part` still has a defensive `text.replace('\0', "")` but every other display-hostile class (`\t`, `\r`, `\n`, the full `Bidi_Control=Yes` set, the zero-widths, U+2028/U+2029) is currently unfiltered on this path on both backends. `wide_ptr_to_string` breaks at the first `\0`, so the `\0` strip is already dead code for embedded NULs. What's live: a plugin that relays attacker-influenced text (branch names, archive contents, an untrusted filename it just opened) into `NPPM_SETSTATUSBAR` without its own filtering — the exact "legitimate plugin, hostile content" scenario the Phase-5 chrome consolidation (`ffa5246`, "Consolidate every filename-to-chrome path on one sanitization policy") was meant to close — can push a bidi override or zero-width run into the status bar. Bounded severity: reaching this requires an already-loaded in-process plugin (a trust level DESIGN.md §6.5 already grants), so it adds no new capability to a genuinely malicious plugin, but it defeats the protection for a well-behaved plugin. Fix routes `text` through `sanitize_display_char` (per-char, uncapped) in both `set_plugin_status` sites, drops the now-redundant `text.replace('\0', "")`, and adds a matching test to each backend. Surfaced by the security audit on the sanitizer test-fix / docstring-consolidation commit.

- **~~The Win32 Workspace tree renders unsanitized filenames, and one of its context-menu actions executes the file.~~ Landed (Phase 5).** `insert_tree_folder_children` used to write `entry.file_name().to_string_lossy()` straight into `TVITEMW.pszText`, so a filename carrying U+202E RIGHT-TO-LEFT OVERRIDE (`invoice\u{202E}fdp.exe`) rendered as `invoice⁩exe.pdf` — a fake PDF whose real extension the "Run by system" context-menu entry then handed to `ShellExecuteW(hwnd, "open", …)`, which for an EXE means *run it*.

  Closed by copying the FIF listview's shape: `WindowState.workspace_tree_names: HashMap<isize, String>` stores the real component name per `HTREEITEM.0`, `tree_insert_item` writes the `sanitize_filename_for_display` output into `pszText`, and `tree_item_full_path` / `tree_locate_path` read the real name from the map instead of parsing `pszText` back — the exact "functional value vs. label" split `handle_fif_listview_dblclk` already used. The map is populated on insert, drained on delete (both `tree_clear_all` and `tree_delete_all_children`, the latter walking the doomed subtree first), so its invariant is "every entry keys on a live tree handle". The existing `starts_with(&workspace_root)` defence-in-depth checks in the double-click and context-menu paths stay — they defend against a different failure mode (a hostile FS returning a component with an absolute-path prefix).

  Verified: `cargo build/clippy/fmt` clean on Windows, the tree-driving flow still needs a Windows runner to exercise end to end (no automated coverage for `SysTreeView32`-driving functions, consistent with the rest of the file). `ui_gtk` still has no workspace tree, so the same trap applies whenever one lands — the ownership model there is the map, not `pszText`-round-trip.

- **Tab click arm/commit should key on buffer identity, not vector index.** `tab_close_armed` and `tab_pin_armed` (Phase 4.6 addition) record a numeric tab index at `WM_LBUTTONDOWN` and check against a same-position hit-test at `WM_LBUTTONUP`. Mouse capture holds the button-down/up pair on the tab strip but does **not** gate keyboard input to the main window, so a plugin `NPPM_*` call, a `Ctrl+W`/`Ctrl+Shift+W` shortcut, or any other keyboard-triggered `Shell::move_tab` / `close_active_tab` between DOWN and UP can reorder or shrink `Shell.tabs` while the arm is live. If a different logical tab slides into the same visual slot at release time, the release hit-test still returns the same index, the arm matches, and the action commits on the wrong buffer. No memory-safety or data-loss impact (`.get()` degrades safely for out-of-range indices, and `Shell::set_pinned` / the close path both revalidate before mutating), but it's a genuine "clicked X, got Y" UX bug for the two mouse gestures involved. Fix keys the arm on `Tab.id` (buffer id, stable across moves) rather than vector index, then re-finds the tab at release by id — symmetric across both `tab_close_armed` and `tab_pin_armed`. Pre-existed for close-X; the pin path inherits the same shape. Worth landing alongside the Phase 5 cross-platform mouse-input abstraction so the fix ships once for both Win32 and the future GTK/Cocoa event routers.

- **~~Toolbar background colour on the themed Win32 toolbar control.~~ Landed (Phase 4).** The `NM_CUSTOMDRAW` / `CDDS_PREPAINT` variant of option (a) landed in `main_wnd_proc`'s `WM_NOTIFY` arm: `FillRect` the reported `nmcd.rc` with `dialog_bg_brush()` (Code++'s established chrome color, `DIALOG_BG = 0xF9F9F9`), return `CDRF_NOTIFYITEMDRAW` so the toolbar hands each button back to UxTheme for its themed hover/pressed/disabled paint. Beats UxTheme cleanly — CustomDraw's PREPAINT stage fires before the theme background fill, so no subclass or theme drop was needed. `TBSTYLE_CUSTOMERASE` was not required; the toolbar already sends `NM_CUSTOMDRAW` unconditionally. Kept the same chrome bar-across all of Code++'s bars converge on one shade, no per-theme drift. The active-tab indicator entry below still tracks its own UxTheme cooperation battle — its fix is separate.

- **~~Active-tab indicator (orange top edge) on the themed Win32 tab strip.~~ Landed (Phase 4).** Path (a) — full owner-draw via `TCS_OWNERDRAWFIXED` + `WM_DRAWITEM` — arrived in two commits: `670bc40` shipped the owner-draw framework (save icon + close-X paint) as prerequisite scaffolding, and `ec19b1f` landed the indicator itself, `paint_tab_item`'s `if active { FillRect(strip_rc, TAB_ACTIVE_INDICATOR) }` block between the background fill and the icon blit. The indicator uses Material orange 400 (`TAB_ACTIVE_INDICATOR = 0x26A7FF`). Subsequent tuning brought the strip to `TAB_ACTIVE_INDICATOR_HEIGHT_PX_HIDPI = 8` inside a `TAB_HEIGHT_PX = 30` cell, with the 20-px icon (`TAB_ICON_DISPLAY_PX`) overwriting the strip inside its own ~20-px-wide column via `AlphaBlend` — the strip stays fully visible across the rest of the tab width, and the icon centre stays aligned with the text centre.

### 7.5 Phase 5 cross-platform parity checklist

A user opening Code++ on Linux or macOS in Phase 5 should see *every* user-visible feature that's already in Win32, behaving the same way. The Phase 5 demo (DESIGN.md §7.2) is the functional gate; this checklist is the explicit work list so nothing slips. It splits cleanly into "already cross-platform — just plug in `UiPlatform`" and "needs new per-platform plumbing."

**Already cross-platform (no Phase 5 design work; the GTK/Cocoa backends inherit it for free by implementing the existing `UiPlatform` trait + `Shell` API):**

- **Session persistence.** `core::session::{Session, Tab, WindowGeometry}` round-trip via `quick-xml`; `Shell::save_session` / `load_session_entries` already drive every backend. `codepp-core`'s and `codepp-shell`'s suites cover the data-shape contract on every CI runner (true since CI widened to `cargo test --workspace`; see the test-discipline note below).
- **Untitled buffer survival** (the `<config_dir>/backup/` mechanism), **dirty saved-file backup**, **`SessionRestoreEntry::DirtyFromBackup`** with mtime-conflict detection, **`Shell::deferred_dialogs`** queue. All in `shell`. `platform::backups_dir()` already resolves the per-OS path.
- **Window size + maximized persistence.** `WindowGeometry` round-trips in session.xml; the platform applies its native equivalent of `SetWindowPos` + `ShowWindow(SW_SHOWMAXIMIZED)`.
- **Tab reorder logic.** `Shell::move_tab` and its 6 unit tests are platform-agnostic. Each backend wires the platform's drag-detection primitive (Win32 subclass + `WM_LBUTTONDOWN`/`MOUSEMOVE`/`LBUTTONUP`; GTK `GestureDrag`; Cocoa `NSResponder` mouse events) to call it.
- **`PendingDialog`** plumbing — `ConfirmReload` and `Error` dialogs are returned by `Shell::drain` and presented by the UI. Each backend just maps the two variants to its native dialog primitive.
- **File loading, encoding/EOL detection, file watching, find-in-files, plugin host, NPPM/NPPN dispatch.** All in `core` / `shell` / `plugin-host`; cross-platform from day one.

**Per-platform work for `ui_gtk` and `ui_cocoa` (the user-visible features Phase 5 must replicate):**

- **Main window chrome.** Menu bar (with mnemonic underlines), status bar (7-part layout matching the Win32 spring), toolbar (32 buttons across 10 separator-delimited groups, with the same SVG icon set under `assets/icons/`), tab strip.
- **Tab drag-to-reorder.** GTK: `GtkEventControllerLegacy` or `GestureDrag` on the `GtkNotebook` tab labels, hit-test → `Shell::move_tab`. Cocoa: `NSTabView` doesn't natively support drag-reorder; either subclass `NSTabViewItem`'s tracking area or build a custom tab strip on top of `NSScrollView` (matches what most Cocoa editors do).
- **Modal dialogs.** Goto, Find/Replace, FIF progress, About (with clickable home-page link + F1 binding), reload-confirm, error. Each platform has its own primitive — `GtkDialog` / `NSAlert` / `NSWindow` modal — but the *content shape* and the *trigger conditions* are documented by the existing Win32 implementations and by `PendingDialog`. Don't reinvent the UX; copy it.
- **Periodic auto-save.** Today on Win32 via `SetTimer` + `WM_TIMER` arm calling `Shell::save_session`. GTK uses `g_timeout_add(7000, …)`; Cocoa uses `NSTimer` scheduled for 7-second intervals. **Suggested abstraction** when the GTK backend lands: a new `UiPlatform::start_periodic(period_ms, callback)` returning a cancellation handle — would let `Shell` own the cadence and remove the timer-id constants from each backend, but keep the `WM_TIMER` arm in Win32 as the single subscriber.
- **Window-size restore.** Each platform applies its equivalent of `SetWindowPos(width, height) + ShowWindow(SW_SHOWMAXIMIZED)` from `Shell::saved_window_geometry()`. The toolbar-floor calculation is shared (`toolbar::natural_min_width_px` returns the inner width; the AdjustWindowRectEx step is platform-specific frame-chrome math that GTK/Cocoa replace with their own).
- **Drag-and-drop file open.** Win32 `DragAcceptFiles` / `WM_DROPFILES`. GTK `gtk_drag_dest_set` + `drag-data-received` signal. Cocoa `NSDraggingDestination` protocol on the main window.
- **Native open / save-as / folder-pick dialogs.** Win32 `GetOpenFileNameW` / `GetSaveFileNameW` / `SHBrowseForFolderW`. GTK `GtkFileChooserNative`. Cocoa `NSOpenPanel` / `NSSavePanel`.
- **Accelerators / hotkeys.** F1 → About, Ctrl+S → Save, Ctrl+W → Close, Ctrl+F/H → Find/Replace, Ctrl+G → Goto, Ctrl++/-/0 → Zoom, F3/Shift-F3 → Find Next/Prev. The list is in the Win32 `CreateAcceleratorTableW` block as the source of truth — GTK uses `GtkApplication::set_accels_for_action`, Cocoa uses `NSMenuItem.keyEquivalent`.
- **Per-tab document binding.** `UiPlatform::activate_tab(idx, scintilla_doc)` — Scintilla's `SCI_SETDOCPOINTER` is the same call regardless of host UI; only the editor-handle plumbing differs. The doc-pointer-swap helpers (`capture_text_from_doc`, `is_doc_dirty`) are mechanically the same on every platform.
- **Application icon, title bar.** Win32 embeds `code++.ico` via `app/build.rs`. GTK uses `gtk_window_set_icon_from_file` (or the GResource bundle). Cocoa picks up the icon from the `.app` bundle's `Info.plist`. The shared `assets/code++.png` source feeds all three (the `tools/codepp-app-icon/generate.py` extension point is documented in the script's docstring).

**Test discipline.** The whole workspace's tests run on every CI runner, so the backend-agnostic logic stays verified continuously. This was *not* true from Phase 0 as earlier revisions of this document asserted: CI ran `cargo test -p codepp-core` only, a line written when `core` was the sole crate with tests, so `shell`'s, `editor`'s and `udl`'s suites were verified on contributors' machines and nowhere else — and `ui_win32`'s 130 tests, being behind `#![cfg(target_os = "windows")]`, ran nowhere at all. Widened to `--workspace` in Phase 5. UI-level tests (e.g. "Phase 5 m1 demo: open the GTK build, drag a tab, restart, observe the new order persisted") are manual, gated on the §7.1 phase-rule end-of-phase demo. Don't bypass the demo.

---

## 8. Hard Performance Constraints

These are non-negotiable and verified at each phase boundary.

| Constraint | Budget | Verification |
| --- | --- | --- |
| Cold start (warm cache) to interactive | < 80 ms | Stopwatch from `WinMain` / `main` to first paint. Logged when `--perf` flag is set. |
| Single keystroke latency (typed char → Scintilla redraw) | < 5 ms p99 | Frame-time log on a 10k-line file. |
| Open 10 MB UTF-8 file | UI never blocks | Worker thread reads; marshal posts buffer to UI thread incrementally if needed. |
| Memory floor (one empty buffer) | < 25 MB **PSS** (proportional set size; private-resident is the fallback where PSS is unavailable) | Linux: `/proc/<pid>/smaps_rollup`. Windows: private working set from `GetProcessMemoryInfo` (`PROCESS_MEMORY_COUNTERS_EX.PrivateUsage`). macOS: `phys_footprint` from `task_vm_info`. |
| Undo history | Bounded, default 1000 ops, configurable | Scintilla `SCI_SETUNDOCOLLECTION` + periodic trim. |
| Plugin load on startup | Zero plugins loaded until first user interaction touches them | Trace log at startup shows zero `LoadLibrary` calls until menu open. |
| File I/O thread | Dedicated worker pool, never UI thread | Lints fail if `core::file` is called synchronously from a UI crate. |

**Why the memory metric is PSS and not RSS.** The original wording said "< 25 MB RSS", which was written and measured against Win32 and does not survive the move to a second platform. RSS counts every resident page a process can see, including read-only, file-backed shared-library text that is demand-paged from the page cache and shared with every other process using the same toolkit. That inflates the number by an amount that varies with how heavy the platform toolkit is, not with anything Code++ does — so the same binary "fails" on one OS and "passes" on another while costing the system an identical amount. PSS attributes shared pages proportionally and is the figure that reflects real cost; on the platforms without a PSS equivalent, private-resident is the closest analogue and is stricter, not looser. The budget number itself is unchanged at 25 MB.

**Measured, Phase 5 m3, release build, one empty buffer, x86_64 Linux + GTK 3.24:**

| Figure | Value | Note |
| --- | --- | --- |
| PSS | **11.5 MB** | the constraint above — 46% of budget |
| Private (clean + dirty) | 10.3 MB | of which 6.3 MB dirty |
| `Shared_Clean` | 27.0 MB | GTK / GDK / pango / cairo / glib library text, shared with every GTK process |
| RSS | 37.3 MB | PSS + the shared pages above; recorded for continuity with the old metric |

The shared-page cost is not avoidable by writing the UI layer differently: `libscintilla.a` alone has 305 distinct undefined `gtk_*` / `gdk_*` / `pango_*` / `cairo_*` / `g_object_*` symbols, because Scintilla's GTK backend is itself a GTK client. Any Linux build that links Scintilla maps the whole stack, whether the Rust above it is gtk3-rs, `gtk-sys`, or hand-written `extern "C"`.

**Where the growth came from — measured, not inferred.** The figure recorded at m1 was 8.5 MB PSS; the table above is 11.5 MB. That is past §8's own ">20% regresses the constraint" bar if read as measurement-over-measurement, even though the absolute figure is under half the budget. An earlier revision of this section guessed the cause ("the likely bulk is the lexer theme table"). That guess was wrong, and the measurement is below.

m1 (`e64f530`) and m3 (`HEAD`) were built and run back to back on the same machine with an empty session, and `/proc/<pid>/smaps` attributed per mapping:

| Bucket | m1 | m3 | Delta |
| --- | --- | --- | --- |
| binary `.text` | 0.89 MB | 3.49 MB | **+2.60 MB** |
| binary `.rodata` | 0.21 MB | 1.06 MB | +0.85 MB |
| shared libs | 2.21 MB | 2.33 MB | +0.11 MB |
| anonymous | 1.30 MB | 1.37 MB | +0.07 MB |
| **heap** | **3.46 MB** | **3.46 MB** | **±0.00 MB** |
| Total PSS | 8.17 MB | 11.81 MB | +3.64 MB |

Three things fall out of that, and only the third matches the old guess:

1. **71% of the growth is code, not data.** The theme table itself is `.rodata` — 85 `const` items that rvalue-promote when `lang_theme` hands out `&'static` references — and accounts for at most the +0.85 MB there, under a quarter of the total. What actually grew is `.text`: the whole of m2 and m3 (the `Shell` wiring, 34 `UiPlatform` methods, the status bar, the menus, session restore, the tab strip) plus the per-language theme *application* code, which is an 85-arm dispatch each issuing ~20 styling calls. Code, not tables.
2. **Heap did not move at all** — 3.46 MB at both ends, to the kilobyte. Nothing added between m1 and m3 allocates more at rest. Whatever this is, it is not a leak and it does not scale with what the user opens.
3. **So it is a one-off static floor rise**, which was the old conclusion, reached for the wrong reason.

Resident `.text` is 3.49 MB of 4.07 MB on disk (88%, up from 74% at m1), so this is not "the binary got bigger and the pages are idle" — startup genuinely executes most of it. Reducing it means executing less code at startup or generating less of it: `lang_theme`'s 85-branch `if` chain compiled to a table lookup, `opt-level = "s"` for the release profile, or `lto = "fat"` over the current `"thin"`. None has been tried; each is a measurable experiment rather than a guess.

**The decision this needs is recorded in §7.4.** The numbers above are the evidence for it: 11.8 MB against a 25 MB budget, with the growth being static code that does not scale with usage, and zero movement at runtime.

**Cold start, measured with `--perf`** (which now exists — `main()` entry to first draw, in-process, so it excludes process spawn and dynamic linking but includes everything Code++ does):

| Session restored | Cold start |
| --- | --- |
| Empty buffer | 39.5 ms |
| One 10 000-line `.rs` file | 49.5–50.1 ms |

Both comfortably inside the 80 ms budget, and the ~10 ms difference is the file load, which is what it should be. This is a different quantity from the exec-to-window-exists figures recorded during m3 (13–22 ms) — that one starts earlier and stops earlier, and the two should not be compared.

**Keystroke latency, characterised.** `--perf` opens the interval at the key press, promotes it when Scintilla reports `SCN_MODIFIED`, and closes it at `SCN_PAINTED`. Measured against a 10 000-line file on a real X11 session with a compositing window manager at 1920×1200/60 Hz — **not** under Xvfb, which has no vsync throttling and where an absent frame-clock floor would prove nothing.

`--perf` also reports the **arrival** distribution — the gap between consecutive keystrokes — because a latency figure cannot be read without the input rate that produced it, and because it is the only way to audit a synthetic input tool. That turned out to matter twice over: see the calibration note below.

| Input rate (measured) | p50 | p90 | p99 | max | samples |
| --- | --- | --- | --- | --- | --- |
| 8 char/s (126 ms gaps, ≈95 wpm — a fast typist) | 1.91 ms | 2.85 ms | **4.62 ms** | 17.8 ms | 5 016 |
| 20 char/s (51 ms gaps, ≈237 wpm — beyond human) | 1.54 ms | 2.67 ms | 64.98 ms | 115.9 ms | 2 992 |

**The §8 budget is met at human typing rates.** p99 is 4.62 ms against 5 ms, backed by ~50 observations rather than the one or two an earlier 88-sample run could offer. The margin is 8%, which is met but not comfortable.

**The tail is not queueing.** An earlier revision said it was, on the grounds that it grew with input rate. It does, but p50 and p90 barely move between the two rows above — p90 *improves* — while p99 goes up 14×. A backlog lifts the whole distribution; this is a sharp cliff affecting about 1% of keystrokes, so something specific and occasional stalls rather than the editor being uniformly slower. Two obvious causes were tested against the same protocol and **both refuted**:

| Hypothesis | Test | p99 | Verdict |
| --- | --- | --- | --- |
| The 7-second session auto-save blocks the UI thread | interval raised to 100 000 s | 65.4 ms | refuted — unchanged |
| Lexilla restyling a large region after an edit | same size file, plain text, no lexer | 60.5 ms | refuted — unchanged |

So the cause is neither the auto-save nor the lexer, and it is content-independent. Identifying it needs a profiler rather than more black-box runs; it is tracked in §7.4. It does not affect the budget verdict, because it does not appear at human rates — the 8 char/s row's *worst* sample was 17.8 ms, so nothing there came close to the cliff.

**Calibration: `xdotool --delay N` delivers a character every N/2 ms, not every N.** Its man page says "delay between keystrokes", which is what an earlier revision of this section believed. Measured two independent ways that agree to 0.1 ms: the in-process arrival distribution reports 125.8 ms at `--delay 250`, and wall-clocking the command externally gives 125.9 ms/char (and 50.9 ms/char at `--delay 100`). **Every input rate this section previously quoted was therefore 2× too low** — what was labelled "4 char/s, ordinary typing" was really ~8 char/s, and "40 char/s" was ~77. The latency figures were never wrong; only the axis they were plotted against. Anyone repeating these measurements should read the arrival row rather than trusting the flag.

**Correcting an earlier version of this section.** A previous revision reported p50 ≈ 6.9 ms and p99 ≈ 13–17 ms and concluded that the budget was unsatisfiable because GTK's vsync-locked frame clock imposed an ~8 ms floor. **That was wrong, and it was an instrumentation bug rather than a finding.** The first implementation kept only the most recent unpainted keystroke, so when several characters arrived within one frame it measured the *last* one and discarded the rest — throwing away the longest waits, reporting a partial frame's delay as if it were the whole distribution, and producing 180 samples where 264 characters had been typed. Per-keystroke sampling shows a p50 nearly ten times lower. The frame-clock reasoning was internally consistent and matched the numbers it was given, which is precisely why a wrong instrument is dangerous: it produced a confident, quantitative, plausible conclusion. The lesson is recorded rather than quietly deleted.

**How the interval is measured, on both platforms.** A key press opens it, Scintilla's `SCN_MODIFIED` promotes it to a real measurement, and `SCN_PAINTED` closes it. The middle step is what makes it honest: plenty of keys repaint nothing — Escape, arrows, Home/End, a Backspace at position 0, anything on a read-only buffer — and a press committed without it would wait until some unrelated later paint, the caret blink included, closed it with a fabricated latency running to hundreds of milliseconds. It is also what makes **Tab, Enter and Backspace measurable**: they edit the buffer, but only sometimes, and an earlier character-class filter had to exclude them wholesale because it could not tell an editing key from an inert one. Ctrl chords stay excluded — §8 budgets a *typed character*, and a paste's redraw cost is a different quantity large enough to dominate the tail — while `Ctrl`+`Alt` is `AltGr` on many layouts and is not a chord.

Verified by driving the real app: ten typed characters give ten samples and stay at ten through ten Escapes, twenty navigation keys, or ten Ctrl chords; adding five Enters, five Backspaces or five Tabs gives fifteen.

GTK reaches both notifications by pulling the boxed `SCNotification` out of the `sci-notify` signal's third `GValue` with `g_value_get_boxed`, so the two platforms measure the identical span rather than approximately the same one. It previously closed on GTK's own `draw` signal, which fires *before* Scintilla paints and so ended the interval a frame's cairo work early. Every hook on the Win32 side is filtered to the main editor's `hwndFrom`, so a plugin's own Scintilla panel cannot consume a pending keystroke — but none of the Win32 half has been run, because it needs a Windows machine.

**On the direct-call path being architecturally identical across platforms:** it is — `ScintillaGTK::DirectFunction` is captured once by `EditorHandle::from_gtk_widget`, so calls *into* Scintilla cost the same on either backend. What differs is input delivery (`key-press-event` rather than `WM_CHAR`) and painting (cairo rather than GDI/Direct2D), neither of which is on the direct-call path. An earlier revision of this paragraph said that difference had never been profiled and warned against assuming parity; the GTK half now has been, and the ~0.7 ms per-keystroke figure above is the result. Win32 remains unmeasured, so the warning still stands in that direction.

If a future feature breaks any of these, it must either be feature-gated (off by default) or be redesigned. No exceptions for "minor" regressions.

---

## 9. Build, Packaging, CI

### 9.1 Build

- `cargo build -p app` produces `code++.exe` (Windows) / `code++` (Linux/macOS).
- All native code (Scintilla, Lexilla) is statically linked. The binary depends only on system libraries (`user32`/`kernel32` on Windows, `libgtk-3` on Linux, `Cocoa.framework` on macOS).
- Release profile: `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, `strip = "symbols"` (debug symbols extracted to a side file for crash decoding).

### 9.2 Distribution

- Windows: a single `.exe` plus a `plugins/` folder. Optional MSI installer in a later phase.
- Linux: tarball + AppImage. `.deb`/`.rpm` later.
- macOS: `.app` bundle with code signing in a later phase.

### 9.3 CI

- **Hosting:** the canonical repository is on Forgejo at <https://git.fiedler.live/tux/code-plus-plus>. A read-only mirror is pushed to GitHub. CI runs on Forgejo Actions only; the GitHub mirror has no workflow.
- **Runners:** three self-hosted Forgejo runners with labels `windows`, `linux`, and `macos`. Both the `build` and `lint` jobs use `runs-on: ${{ matrix.runner }}` to fan out across all three labels. The `fmt` and `cargo-deny` jobs run on the `linux` runner only — rustfmt is deterministic across platforms and cargo-deny inspects the workspace manifest, neither benefits from re-running per OS.
- **Why clippy on every platform:** the codebase's per-OS code is heavy (`ui_win32` is Windows-only today; `ui_gtk`/`ui_cocoa` join in Phase 5). A Linux-only clippy run silently accepts dead Windows-cfg code on the Linux/macOS paths; a Windows-only clippy run does the symmetric thing for the Linux/macOS UI backends. Running clippy on every platform means a missing cfg gate or a stale platform-specific lint produces a CI failure on the platform that observes the dead code, not at Phase 5 bring-up time.
- **Required jobs:** `cargo build --workspace --all-targets` on each runner, `cargo test --workspace --all-targets` on each runner, `cargo clippy --workspace --all-targets -- -W clippy::pedantic -D warnings` on each runner, `cargo fmt --check` (linux), `cargo deny check` (linux). The `clippy::pedantic` lint group is gated as denied — every pedantic finding must be explicitly addressed (hand-fixed or suppressed at the smallest reasonable scope with a documented rationale; bulk-category file-level `#![allow(...)]` blocks are accepted where they reflect a structural design choice — see e.g. the FFI-cast allows in `ui_win32` / `editor` / `plugin-host` / the plugin crates).
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
