//! Code++ entry point. Selects the UI backend at compile time.
//!
//! Accepts an optional path to open at startup — same effect as
//! drag-and-dropping that file onto the window, exercising the
//! identical Loader → wake → drain → Scintilla pipeline, which is
//! useful for demo verification scripts that cannot simulate
//! cross-process drag-drop (HGLOBAL is per-process) — plus the
//! diagnostic flags DESIGN.md §5.5 and §8 specify. See [`args`] for
//! the grammar and [`logging`] for the sink.
//!
//! # Backend selection
//!
//! Which `ui_*` crate is linked is decided by a cargo feature (`win32`
//! / `gtk`), both of which are on by default — see this crate's
//! `Cargo.toml` for why enabling both is safe. The `cfg` arms below
//! pair each feature with its target so the combination can never
//! resolve to two backends, and so a target with no backend yet
//! (macOS, until the Cocoa work lands) still builds and exits with a
//! clear message rather than failing to link.

// `ExitCode` is used by every arm. Paths are spelled inline where
// needed rather than imported, so a backend-less target (macOS until
// the Cocoa arm lands) does not warn on an unused import.
use std::process::ExitCode;

mod args;
mod logging;

/// Parse arguments and install the log sink, or report and exit.
///
/// Runs on every target, backend or not, so `--help` and a bad flag
/// behave the same everywhere rather than only where a UI is linked.
/// `Err(code)` means "we are done, exit with this".
fn startup() -> Result<args::Args, ExitCode> {
    // Skip argv[0].
    match args::parse(std::env::args_os().skip(1)) {
        args::Parsed::Run(parsed) => {
            let want_logs = parsed.verbose;
            let installed = logging::init(want_logs);
            if parsed.perf && !installed {
                // The measurements are still *taken* — `Perf` is
                // enabled either way — but `report()` writes through
                // `tracing`, so with no subscriber they are recorded
                // and then discarded. Silence would otherwise read as
                // "measured, nothing to report".
                eprintln!("Code++: --perf was requested but no log sink could be installed;");
                eprintln!("Code++: the measurements will be taken and then discarded.");
            }
            Ok(*parsed)
        }
        args::Parsed::Exit(text) => {
            print!("{text}");
            Err(ExitCode::SUCCESS)
        }
        args::Parsed::Fail(text) => {
            eprintln!("Code++: {text}");
            Err(ExitCode::FAILURE)
        }
    }
}

// The `cfg` predicate below — "some backend is linked" — is repeated
// rather than aliased because `cfg(...)` does not accept macro
// expansion; keep the copies in sync when the Cocoa arm lands.

/// Collapse a backend's `Result` into a process exit code, reporting
/// any error on stderr. Shared by both backends so their failure
/// reporting cannot drift apart.
#[cfg(any(
    all(target_os = "windows", feature = "win32"),
    all(target_os = "linux", feature = "gtk")
))]
fn finish<E: std::fmt::Display>(result: Result<(), E>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Code++: fatal error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(target_os = "windows", feature = "win32"))]
fn main() -> ExitCode {
    // First statement, before argument parsing and before the log
    // sink: everything after this point is time a user waits through.
    // `enabled` is patched in once the flags are known — the clock has
    // to start before we know whether anyone asked for it.
    let started = std::time::Instant::now();
    let parsed = match startup() {
        Ok(a) => a,
        Err(code) => return code,
    };
    let perf = codepp_core::perf::Perf::started_at(started, parsed.perf);
    finish(codepp_ui_win32::run(parsed.path, perf))
}

#[cfg(all(target_os = "linux", feature = "gtk"))]
fn main() -> ExitCode {
    // See the Win32 arm for why the clock starts here.
    let started = std::time::Instant::now();
    let parsed = match startup() {
        Ok(a) => a,
        Err(code) => return code,
    };
    let perf = codepp_core::perf::Perf::started_at(started, parsed.perf);
    finish(codepp_ui_gtk::run(parsed.path, perf))
}

#[cfg(not(any(
    all(target_os = "windows", feature = "win32"),
    all(target_os = "linux", feature = "gtk")
)))]
fn main() -> ExitCode {
    // Still parse first, so `--help` and a mistyped flag report
    // properly on a platform whose backend has not landed.
    let parsed = match startup() {
        Ok(a) => a,
        Err(code) => return code,
    };
    let _ = parsed.path;
    eprintln!(
        "Code++ has no UI backend for this platform/feature combination. \
         The Cocoa backend lands in Phase 5 — see DESIGN.md §7.2."
    );
    ExitCode::FAILURE
}
