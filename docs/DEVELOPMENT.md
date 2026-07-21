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
- During setup, leave the line-ending option at the default ("Checkout as-is, commit Unix-style") — this matches the `.gitattributes` in the repo.

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
cargo test -p codepp-scintilla-sys -- --ignored             # on a desktop
xvfb-run cargo test -p codepp-scintilla-sys -- --ignored     # headless
```

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

---

## 4. macOS

macOS support is part of Phase 5. Until then, macOS contributors can build the headless crates.

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

### 4.4 No extra system libraries pre-Phase-5

Cocoa is provided by the SDK that comes with the Command Line Tools. Phase 5 will add `objc2` and `objc2-app-kit` as Cargo dependencies; nothing to install at the system level.

### 4.5 Verify

```sh
clang --version
rustc --version
cargo --version
git submodule status
cargo build --workspace
```

---

## 5. Common Tasks After Setup

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
cargo check --workspace --target aarch64-apple-darwin    # fully clean today
cargo check -p codepp-core --target x86_64-pc-windows-msvc
```

The macOS target is the useful one: `crates/scintilla-sys/build.rs`
takes its no-native-backend arm there, so nothing needs a C toolchain
and the whole workspace checks. The Windows target will get as far as
`cc-rs` and then fail on a missing `windows.h` unless you have a
Windows SDK — that failure is expected and still informative, because
everything before it (build-script compilation, all Rust type
checking) has already succeeded by then.

To typecheck the Windows-only crates — `ui_win32` is the largest in the
workspace and is `#![cfg(target_os = "windows")]`, so a Linux `cargo
build` compiles it to an *empty* rlib and verifies nothing — set
`CODEPP_SKIP_NATIVE_BUILD=1`. That makes `crates/scintilla-sys/build.rs`
skip compiling the vendored C/C++, which is the only thing stopping a
cross-target check on a machine with no Windows SDK:

```sh
CODEPP_SKIP_NATIVE_BUILD=1 cargo check -p codepp-ui-win32 --all-targets \
    --target x86_64-pc-windows-msvc
```

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
