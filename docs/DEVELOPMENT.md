# Code++ — Development Environment Setup

This document walks through everything that must be installed on each supported platform to get a working development environment for Code++. It is the source of truth for "how do I get from a fresh OS to `cargo run -p app`."

If a step here is wrong or out of date, fix it in this file in the same commit that fixes the underlying issue. Setup docs that lie waste more time than no setup docs at all.

---

## 1. What You Need on Every Platform

Independent of OS, every contributor needs:

- **Rust toolchain** managed by `rustup`, on the channel pinned in `rust-toolchain.toml` at the repo root. Never install Rust through the system package manager — distro-shipped Rust drifts and breaks `cc`-driven builds.
- **Git** with submodule support. Code++ vendors Scintilla and Lexilla as submodules, so `git clone` alone is not enough.
- **A C/C++ toolchain** capable of compiling Scintilla 5.x. The exact toolchain differs per platform (see below) but the requirement does not — `crates/scintilla-sys/build.rs` invokes the `cc` crate, which expects a working host compiler.
- **`pkg-config`** on Linux and macOS (Windows uses MSVC's own resolution). Required to find GTK and other system libraries from `build.rs`.

Once those are present, the cross-platform bring-up is identical:

```sh
git clone --recurse-submodules https://git.fiedler.live/tux/code-plus-plus.git
cd code-plus-plus
cargo build --workspace
cargo run -p app
```

If you cloned without `--recurse-submodules`:

```sh
git submodule update --init --recursive
```

The first build compiles vendored Scintilla and Lexilla from C/C++ source. Expect 1–3 minutes on first build, seconds on incremental builds.

---

## 2. Windows

Windows is the **primary development platform** through Phase 4. Get this one working first.

### 2.1 Visual Studio Build Tools 2022 (or Visual Studio 2022)

Required for the MSVC C++ compiler, the Windows 10/11 SDK, and the linker. Scintilla's Win32 backend is C++, so `cl.exe` and `link.exe` must be on PATH for `cc` to find them.

- Download: https://visualstudio.microsoft.com/downloads/ (Build Tools or Community edition — both work).
- Workloads to select in the installer:
  - **Desktop development with C++**
- Individual components to confirm are checked:
  - MSVC v143 (or later) — VS 2022 C++ x64/x86 build tools
  - Windows 11 SDK (latest)
  - C++ CMake tools for Windows (optional but useful)

After install, open a **Developer Command Prompt for VS 2022** or a **Developer PowerShell** so `cl.exe` is on PATH. Plain PowerShell will not work for `cargo build` unless you have separately initialized the MSVC environment.

### 2.2 Rust toolchain

- Install `rustup`: https://rustup.rs/ → run `rustup-init.exe`.
- When prompted for the host triple, accept the default `x86_64-pc-windows-msvc`. **Do not pick the GNU toolchain** — Code++ targets MSVC.
- After install: `rustup default stable`, then `rustup show` to verify.

The repo's `rust-toolchain.toml` will pin a specific stable version on first `cargo` invocation; rustup downloads it automatically.

### 2.3 Git

- Install Git for Windows: https://git-scm.com/download/win.
- On the installer's line-ending page, pick "Checkout as-is, commit Unix-style line endings" (`core.autocrlf=input`). It is not the default: that is "Checkout Windows-style, commit Unix-style line endings" (`core.autocrlf=true`), which checks text files out with CRLF, and a shell script with CRLF line endings breaks in bash. Rust sources are LF whichever you pick: `.gitattributes` pins `*.rs` to LF, because source-scan tests read their own file with `include_str!` and must see the same bytes on every machine. A clone made before that rule keeps its CRLF `.rs` files until they are next rewritten.

### 2.4 Verify

In a Developer PowerShell:

```powershell
cl                          # should print "Microsoft (R) C/C++ Optimizing Compiler"
rustc --version             # should print rustc 1.x.y (...)
cargo --version
git --version
git submodule status        # should list scintilla and lexilla under crates/scintilla-sys/vendor/
cargo build --workspace     # full build
cargo run -p app            # launches the app
```

If `cl` is not found, you opened the wrong shell. Use the **Developer** PowerShell, not the regular one.

### 2.5 Optional but recommended

- **Windows Terminal** for a usable shell experience.
- **VS Code** with `rust-analyzer` and `CodeLLDB` extensions — debugging native Rust + Win32 is much easier with a real debugger.
- **Sysinternals Process Explorer** for the Phase 1 demo verification (memory and DLL load checks).

### 2.6 Developer Mode (needed for the FIF symlink adversarial tests)

The find-in-files worker's per-file open uses
`FILE_FLAG_OPEN_REPARSE_POINT` on Windows so a path swapped for a
symlink or junction between enumeration and open is refused rather
than followed. The regression tests for that guard need to *create*
a symlink at test-setup time, which requires
`SeCreateSymbolicLinkPrivilege` — granted to processes running under
Windows Developer Mode, or to elevated shells.

Two tests are `#[ignore]`d for this reason:

  * `codepp_shell::fif::tests::read_capped_refuses_a_symlink`
  * `codepp_shell::fif::tests::atomic_write_cannot_be_redirected_by_a_planted_temp_symlink`

The default `cargo test` run reports them as ignored — visible in the
per-target summary line — so a runner in the wrong state can't
silently drop coverage. To exercise them, enable Developer Mode
(**Settings → System → For developers → Developer Mode**, or from an
**elevated** PowerShell — the `HKLM` write requires it, same shell
class the §2.4 `cl` note already establishes:
`reg add HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock
/v AllowDevelopmentWithoutDevLicense /t REG_DWORD /d 1 /f`) and run:

```powershell
cargo test -p codepp-shell --lib -- --include-ignored
```

The self-hosted `windows`-label CI runner should have Developer Mode
enabled and run the workspace tests with `--include-ignored`. Without
that, the `FILE_FLAG_OPEN_REPARSE_POINT` arm in `read_capped` has no
active regression coverage — the sibling `read_capped_refuses_a_junction`
test still runs (directory junctions don't need the privilege), but
it exercises a different reject path (`ERROR_ACCESS_DENIED` on the
directory open) and would still pass even if the flag were removed.

---

## 3. Linux

Linux support lands in Phase 5. As of Phase 5 m1 the Linux build compiles real Scintilla against GTK 3, so `libgtk-3-dev` (or your distro's equivalent) is a **hard requirement** for `cargo build --workspace` — not an optional extra. GTK 3 rather than GTK 4 because Scintilla has no GTK 4 backend; see DESIGN.md §4.1.

The instructions below cover Ubuntu 24.04 / Debian 12. Translate to your distro's package names as needed.

### 3.1 System packages

```sh
sudo apt update
sudo apt install -y \
    build-essential \
    pkg-config \
    git \
    curl \
    libgtk-3-dev \
    libglib2.0-dev \
    libpango1.0-dev \
    libcairo2-dev \
    libgdk-pixbuf-2.0-dev
```

- `build-essential` provides `gcc`, `g++`, `make`, and `libc6-dev` — required by `cc` for Scintilla.
- `libgtk-3-dev` and its companions are required from Phase 5 m1 onward — `crates/scintilla-sys/build.rs` probes `gtk+-3.0` via `pkg-config` and fails the build if it is missing. Self-hosted CI runners with the `linux` label need it installed too.

On Fedora:

```sh
sudo dnf install -y gcc gcc-c++ make pkgconf-pkg-config git curl gtk3-devel
```

On Arch:

```sh
sudo pacman -S --needed base-devel pkgconf git curl gtk3
```

### 3.2 Rust toolchain

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup default stable
```

Do not install `rustc` or `cargo` from `apt`/`dnf`/`pacman` — distro packages lag and conflict with `rustup`'s pinning.

### 3.3 Verify

```sh
gcc --version
g++ --version
pkg-config --modversion gtk+-3.0   # required: scintilla-sys probes this on Linux
rustc --version
cargo --version
git submodule status
cargo build --workspace
cargo run -p codepp-app          # opens the GTK window
```

The FFI smoke test that proves Scintilla is really linked and really
working needs a display, so it is marked `#[ignore]` and does not run in
the default `cargo test`. Run it explicitly:

```sh
cargo test -p codepp-scintilla-sys -p codepp-ui-gtk -- --ignored
xvfb-run cargo test -p codepp-scintilla-sys -p codepp-ui-gtk -- --ignored
```

`ui_gtk` carries display-gated scenarios for the same reason: they drive
a real Scintilla widget to pin the doc-pointer discipline that lets one
view serve many tabs, the print-export path, and the cross-thread
`SCI_*` marshal. They all run from **one** `#[test]`,
`display_tests::gtk_display_scenarios`, and a new scenario must be added
to it rather than given a `#[test]` of its own.

That is a hard requirement, not a tidiness preference. `gtk::init()`
records the thread that first called it and *panics* on a call from any
other, and libtest runs every `#[test]` on its own spawned worker —
including at `--test-threads=1`, which serialises tests without pinning
them to a thread. A second display-gated `#[test]` therefore fails
outright with `Attempted to initialize GTK from two different threads`,
deterministically rather than intermittently. This is not hypothetical:
these scenarios were previously three separate tests carrying a
`--test-threads=1` instruction, and two of the three failed on every run
until Phase 5 consolidated them.

### 3.4 Self-hosted CI runner provisioning

The `linux`-labelled Forgejo runner needs `libgtk-3-dev` installed on
the host. `.forgejo/workflows/ci.yml` deliberately installs nothing (see
DESIGN.md §9.3 — no `actions/cache`, no third-party setup actions), so
this is a one-time manual step per runner. Without it, `cargo build
--workspace` fails in `crates/scintilla-sys/build.rs` at the
`pkg_config` probe.

Expect the Linux runner's first build after this change to take 1–3
minutes longer: it now compiles the vendored Scintilla and Lexilla C++
sources, which it previously skipped.

The runner must also run the tests as an ordinary account, not as root.
Two of the plugin-panel key tests need a file their own account cannot
read, and root reads every file. Run as root they skip on a
developer's machine, but they *fail* when `CI` is set: `cargo test`
hides a passing test's output, so a skip there would drop the coverage
while the run stayed green — the trap §2.6 describes for the Windows
symlink tests.

---

## 4. macOS

macOS support landed in Phase 5: the Cocoa backend builds, runs and hosts plugins, and `cargo run -p codepp-app` opens it.

### 4.1 Xcode Command Line Tools

```sh
xcode-select --install
```

Provides `clang`, `make`, `git`, and the macOS SDK. The full Xcode app is **not** required.

### 4.2 Homebrew (optional but standard)

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

### 4.3 Rust toolchain

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup default stable
```

`rustup` selects `aarch64-apple-darwin` on Apple Silicon and `x86_64-apple-darwin` on Intel — both are supported.

### 4.4 No extra system libraries

Nothing to install beyond the Command Line Tools. Cocoa and QuartzCore
come from the SDK they ship, and `crates/scintilla-sys/build.rs` links
them by name — macOS has no pkg-config equivalent for system
frameworks, so unlike the Linux path there is no dev package to add.
`objc2`, `objc2-app-kit` and `objc2-foundation` are ordinary Cargo
dependencies and need no system-level setup.

From Phase 5 m1 the macOS build compiles the vendored Scintilla Cocoa
backend (`vendor/scintilla/cocoa/*.mm`) as Objective-C++, so the first
build takes 1–3 minutes longer than it used to — it previously skipped
the native build entirely and produced an empty rlib.

### 4.5 Verify

```sh
clang --version
rustc --version
cargo --version
git submodule status
cargo build --workspace
cargo run -p codepp-app          # opens the Cocoa window
```

The smoke test that proves Scintilla is really linked and really
working drives a real `NSView` hierarchy, so it needs a window server
and is opt-in — the same convention as the GTK display tests in §3.3:

```sh
cargo test -p codepp-ui-cocoa --test cocoa_smoke -- --ignored
```

A plain `cargo test` reports it as ignored in the summary line rather
than skipping it silently, so a runner without a window session cannot
drop the coverage while still looking green.

It runs five scenarios from one `main` — the direct-call round trip,
notification delivery, the cross-thread `SCI_*` marshal (a plugin
worker thread's message hopping onto the main queue), plugin dock
panels hosted by a real dock (a plugin's `NSView` adopted, refused,
hidden and taken back), and what a plugin asks the host to make (a
Scintilla view of its own, a toolbar button, a modeless-dialog
registration). A new display-gated scenario belongs in
`smoke::run` beside them, for the same reason the GTK scenarios share
one `#[test]` (§3.3); the private items it needs are reached through
the crate's `#[doc(hidden)]` `smoke_support` re-export rather than by
moving the scenario in-crate, where libtest could never hand it the
main thread. That surface exists only in debug builds, so a
`--release` run of the smoke binary reports the last three scenarios as
ignored.

Launched from a **non-interactive** shell (an agent session, `ssh`,
a CI runner with no GUI login), `NSApplication::sharedApplication`
can block for minutes before the first scenario starts — measured at
four minutes here, with the scenarios then completing in about a
second. The stall precedes every line of test code, so it is not a
hang in the scenarios; run it from a terminal inside the GUI session
if the wait matters.

Note it is a `harness = false` test. That is load-bearing: AppKit is
main-thread-only and **libtest never yields the main thread** — it runs
every `#[test]` on a spawned worker even at `--test-threads=1`
(measured, not assumed). Replacing the harness gives the test file its
own `main`, which cargo runs on the real main thread. `ui_gtk` does not
need this because GTK only requires all its calls happen on *one*
thread, not specifically the first.

Two harmless `NSLog` lines appear on every macOS run:

```text
Wait cursor is invalid.
Reverse arrow cursor is invalid.
```

They come from vendored source (`cocoa/ScintillaView.mm:1255`, `:1264`)
looking for cursor images in a framework bundle's resources. Code++
static-links Scintilla rather than shipping its framework, so those two
cursors fall back to system defaults. Cosmetic; not a build problem.

The `macos`-labelled runner must run the tests as an ordinary account,
not as root, for the reason §3.4 gives for Linux: the plugin-panel key
tests (`codepp-platform`'s `panel_key`, shared by both platforms) need a
file their own account cannot read, and fail rather than skip when `CI`
is set. The macOS-only key tests also call `/bin/chmod +a` to put
extended ACLs on files in a temporary directory, so the runner's temp
directory must be on a volume that keeps ACLs (APFS does).

---

## 5. Common Tasks After Setup

### Add or change an `NPPM_*` / `NPPN_*` / docking constant

Verify the number against Notepad++'s published headers, mechanically:

```sh
python tools/npp-abi-check/check.py
```

It fetches upstream's `Notepad_plus_msgs.h` and diffs both places
Code++ declares the message ABI — `crates/plugin-host/src/dispatch.rs`
and `plugins/nppcompat-headers/Notepad_plus_msgs.h` — reporting any
name whose number disagrees. A second pass fetches upstream's
`Docking.h` and `dockingResource.h` and diffs the docking constants
(`DMN_*`, `DWS_*`, `CONT_*`, `DOCKCONT_MAX`) in
`plugins/nppcompat-headers/Docking.h`, `crates/plugin-host/src/ffi.rs`
and `crates/plugin-sdk/src/lib.rs`. For an offline run pass local
copies with `--header`, `--docking-header` and `--docking-resource`.

This is not optional diligence. Six message numbers were wrong for
two phases, three of them landing on *other* real Notepad++ messages,
and the in-repo lock test could not see it because it pins the Rust
constants against the same hand-written literals. The docking family
then turned out to have drifted the same way — `DMN_FIRST` was
`0x1000` against upstream's 1050, so no `DMN_*` the host sent was
recognisable — because the checker did not read the docking headers
yet. An in-tree plugin compiled against the same constants cannot
catch this; only a comparison with upstream (or a probe plugin
loaded into a real Notepad++) can. A wrong number is
not an unanswered message — it is a different message answered
confidently. See DESIGN.md §7.4.

Nothing from upstream is vendored: the tool fetches, prints names and
integers, and writes nothing into the tree.

### Update vendored Scintilla / Lexilla

**Source provenance.** Lexilla's canonical git source is
`https://github.com/ScintillaOrg/lexilla.git` — the official mirror maintained
by the Scintilla project. Scintilla itself does **not** have an official git
mirror; its canonical source is on SourceForge using Mercurial, which cannot
be a git submodule. We therefore use `https://github.com/mirror/scintilla.git`,
a community Mercurial-to-git auto-bridge that has tracked Scintilla releases
for years. The submodule is pinned to a specific commit SHA, which protects
against history rewrites at our pinned point. **Whenever the Scintilla
submodule is bumped, verify the new commit's tree against the upstream tarball
from <https://www.scintilla.org/ScintillaDownload.html>** — diff the source
tree and reject the bump if there is any unexpected divergence.

The submodules pin specific Scintilla and Lexilla release tags. To bump:

```sh
cd crates/scintilla-sys/vendor/scintilla
git fetch --tags
git checkout rel-X-YY-Z
cd ../lexilla
git fetch --tags
git checkout rel-X-YY-Z
cd ../../../..
cargo build -p scintilla-sys     # recompile against new source
cargo test -p scintilla-sys      # smoke test must pass
```

Commit the submodule pointer bumps in the same commit that adapts any code to API changes.

### Catch cross-platform breakage before pushing

CI fans out across three runners, so a change that only builds on your
host burns a full CI cycle to tell you. `cargo check --target` catches
most of it locally in seconds — no cross-linker needed, because `check`
does not link:

```sh
rustup target add aarch64-apple-darwin x86_64-pc-windows-msvc
CODEPP_SKIP_NATIVE_BUILD=1 cargo check --workspace --all-targets \
    --target aarch64-apple-darwin
CODEPP_SKIP_NATIVE_BUILD=1 cargo check -p codepp-ui-win32 --all-targets \
    --target x86_64-pc-windows-msvc
```

`CODEPP_SKIP_NATIVE_BUILD=1` is what makes this work from another host.
`cargo check` still runs build scripts, and
`crates/scintilla-sys/build.rs` compiles the vendored Scintilla and
Lexilla for the *target*: the Win32 backend needs a Windows SDK, and
the Cocoa backend is Objective-C++ that needs Apple's clang and the
macOS SDK. The variable skips that compile. Without it, only the crates
that do not depend on `codepp-scintilla-sys`, such as `codepp-core`,
`codepp-platform` and `codepp-udl`, check cleanly, and everything else
stops in that build script. From Windows, for example, a macOS check
fails there with `failed to find tool "c++"`.

The backends are what these checks are for. `ui_win32`, the largest
crate in the workspace, and `ui_cocoa` are each gated on their own OS,
so a build anywhere else compiles them to an *empty* rlib and verifies
nothing. The Windows check names `codepp-ui-win32` rather than the
workspace because `codepp-app`'s and the plugins' build scripts compile
a Windows resource with `rc.exe` for that target, which the variable
does not skip. `ui_gtk` cannot be checked this way: the GTK bindings'
own build scripts, which the variable does not reach, need the target's
GTK development files through `pkg-config`, so check it on Linux, or in
a Linux container. The same holds for `codepp-platform` on a Linux
target, and so for nearly every crate above it: its plugin panel key
takes its HMAC from GLib through `glib-sys`, whose build script needs
the target's GLib development files.

`cargo check` never links, so skipping the native build costs nothing
there. Anything that *does* link — a binary or a test target — fails
with unresolved `scintilla_*` symbols instead, and the build script
hard-errors if it sees the variable alongside `CI`. Never set it in a
workflow or runner environment.

**This does not cover host-conditional dependencies.** Cargo matches
`[target.'cfg(...)'.build-dependencies]` against the **host** triple,
not the `--target` one, because build scripts run on the host. A build
dependency gated that way is present on your machine and absent on a
runner with a different OS, and no amount of `--target` checking on one
host will reveal it — this bit `pkg-config` in Phase 5 m1 and broke
both the macOS and Windows runners with `E0433: cannot find module or
crate pkg_config` while Linux stayed green. Declare build dependencies
unconditionally and branch on `CARGO_CFG_TARGET_OS` inside `build.rs`
instead; that variable describes the *target*, which is what such
decisions actually want.

### Diagnostics and performance measurement

```sh
codepp --help                 # flags and environment
codepp --verbose              # turn the tracing sink on at `info`
CODEPP_LOG=codepp_shell=debug codepp   # per-crate filter; no flag needed
codepp --perf file.rs         # log the DESIGN.md §8 measurements
```

The sink is **off** unless asked for, so the ~230 `tracing::` call sites
cost nothing in a normal run (DESIGN.md §5.5). Before this existed there
was no subscriber at all and every one of them was silently discarded.

`CODEPP_LOG` *overrides* `--verbose` rather than adding to it, so
combining a narrow filter with `--perf` silently drops the
measurements — name the target explicitly if you want both:
`CODEPP_LOG=codepp_shell=debug,codepp::perf=info codepp --perf`.

`--perf` reports cold start (`main()` to first draw) once, and the
keystroke-latency distribution **at exit** — so quit the window
normally rather than killing the process, or the distribution is lost.

**Delete `session.xml` before measuring memory or cold start.** Code++
restores the previous session by design, so launching the binary and
reading `phys_footprint` measures whatever was last open — which drifts
upward over a working day and is not the "one empty buffer" figure
DESIGN.md §8 budgets. Measured on macOS: 18.5 MB clean, 29.3 MB with one
10 000-line file open, 76 MB with an accumulated five-file session. That
mistake stood in §8 for four milestones.

`--perf` reports two distributions: keystroke latency, and the **gap
between arriving keystrokes**. Read the second before the first — a
latency p99 means something different at 8 characters per second than
at 20, and it is the only way to know what an input tool actually
delivered. `xdotool type --delay N` types a character every *N/2* ms
despite its man page, so trusting the flag understates the rate by 2×.

Read DESIGN.md §8 before drawing conclusions. It records the measured
figures on both backends now — GTK at p99 4.62 ms at a fast-human rate,
inside the 5 ms budget; Win32 at p99 ≈ 7–9 ms and cold start ≈ 94 ms,
both outside their budgets — plus the GTK tail above ~15 char/s whose
cause is still unidentified and three retracted earlier conclusions.

All three backends close the cold-start interval on Scintilla's
`SCN_PAINTED`, so the figures are the same quantity. macOS did **not**
until Phase 5 m4b: it marked before `-[NSApplication run]`, which is
before the window is ordered front and before anything paints, and its
m1–m3a figures are retracted for that reason. macOS's figures arrived in
Phase 5 m4b, from a local `NSEvent` monitor: p99 ≈ 20 ms, 4x the
budget and the worst of the three. That one is profiled and
partly fixed: every keystroke repaints every visible line, and a
`CTLine` cache in `crates/scintilla-sys/cxx/QuartzTextLayout.h` took a
third off it (p99 15.6 ms at 64 visible lines, 4.0 ms at 14). Latency
scales with window height; §8 lists five refuted causes so they are not
re-tried.

To reproduce the Win32 numbers on a Windows session, `SendKeys.SendWait`
from a PowerShell harness is what those measurements used — the
tool's sync-per-key round trip is ~35 ms on its own, so a
`Start-Sleep -Milliseconds 100` between calls lands near the ≈7 char/s
"fast typist" band, and `Start-Sleep -Milliseconds 15` saturates near
20 char/s (the tool's own delivery cost bounds it, so the sleep
below ~15 ms stops mattering). Drive the process's
`MainWindowHandle` rather than `FindWindowW`; that avoids depending
on the main-window class name (`CodePlusPlusMainWindow` at the time
of writing) and works even if the process took a moment to register
its class. Quit with a `PostMessageW WM_CLOSE`, not `Stop-Process` —
the distribution is emitted at pump-exit and is lost on a kill.

To drive a Win32 *dialog* from a script rather than the editor,
three things make it deterministic. Launch the binary with `APPDATA`
pointed at a scratch directory (the child process's environment
only), so the run restores a `session.xml` you wrote and never
touches the real one. Post `WM_COMMAND` with the menu id to
`MainWindowHandle` instead of sending keystrokes — no focus games,
and it works from a non-interactive session. Read a `MessageBoxW`'s
body through `GetDlgItem(hwnd, 0xFFFF)` and `GetWindowTextW` (declared
`CharSet.Unicode`; the ANSI marshal truncates at the first NUL) and
dump it as code points, since a bidi override is invisible in a
console. This is how the `show_error_dialog` sanitization was checked
against a pre-change build (DESIGN.md §7.4).

### Run a single phase's demo

Each phase in DESIGN.md §7.2 has a Demo column. The current phase's demo is always reachable via:

```sh
cargo run -p app
```

with manual steps from the Demo description. Automated phase-demo scripts live in `tools/phase-demos/` (added per phase).

### Run the Notepad++ plugin compatibility harness (Windows, Phase 3+)

```powershell
cargo run -p npp-plugin-test -- --plugin path\to\NppExec.dll
```

The harness loads the plugin, calls each required entry point, and asserts the lifecycle messages fire in the correct order. CI runs this with a curated set of public N++ plugins from Phase 3 onward.

---

## 6. Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `error: linker 'link.exe' not found` on Windows | MSVC not on PATH | Open a **Developer** PowerShell, not a plain one. |
| `gtk+-3.0 not found` / `pkg-config exited with status code 1` on Linux | `libgtk-3-dev` missing | Install GTK dev packages from §3.1. |
| `error: failed to run custom build command for scintilla-sys` | Submodules not initialized | `git submodule update --init --recursive`. |
| Build is slow on every `cargo build` | Incremental compilation off, or full rebuild of Scintilla each time | Confirm no `cargo clean` in your loop; Scintilla object files are cached in `target/`. |
| `cargo run -p app` opens window then exits immediately | Phase 0 demo behavior — empty window with File→Exit menu only | Expected before Phase 1. |
| Plugin DLL fails to load with "not a valid Win32 application" | Plugin built for x86, app built for x64 (or vice versa) | Rebuild the plugin for the matching architecture. |

For anything not listed: file an issue with the full output of `cargo build --workspace -vv`, your platform, and your rustc/toolchain version.
