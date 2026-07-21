//! Code++ entry point. Selects the UI backend at compile time.
//!
//! Optional CLI argument: a single path to open at startup. Same effect
//! as drag-and-dropping that file onto the window — exercises the
//! identical Loader → wake → drain → Scintilla pipeline. Useful for
//! Phase 2 demo verification scripts that can't simulate cross-process
//! drag-drop (HGLOBAL is per-process).
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

// `ExitCode` is used by every arm; `PathBuf` only by the
// backend-linked ones, so it is spelled inline in `initial_path`
// rather than imported here (an unused import would warn on a
// backend-less target).
use std::process::ExitCode;

// The `cfg` predicate below — "some backend is linked" — is repeated
// rather than aliased because `cfg(...)` does not accept macro
// expansion. Both helpers carry it so a backend-less build (macOS
// today) does not trip `dead_code`; keep the three copies in sync when
// the Cocoa arm lands.

/// Read the optional startup path from `argv[1]`.
#[cfg(any(
    all(target_os = "windows", feature = "win32"),
    all(target_os = "linux", feature = "gtk")
))]
fn initial_path() -> Option<std::path::PathBuf> {
    std::env::args_os().nth(1).map(std::path::PathBuf::from)
}

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
    finish(codepp_ui_win32::run(initial_path()))
}

#[cfg(all(target_os = "linux", feature = "gtk"))]
fn main() -> ExitCode {
    finish(codepp_ui_gtk::run(initial_path()))
}

#[cfg(not(any(
    all(target_os = "windows", feature = "win32"),
    all(target_os = "linux", feature = "gtk")
)))]
fn main() -> ExitCode {
    eprintln!(
        "Code++ has no UI backend for this platform/feature combination. \
         The Cocoa backend lands in Phase 5 — see DESIGN.md §7.2."
    );
    ExitCode::FAILURE
}
