//! Code++ entry point. Selects the UI backend at compile time.
//!
//! Optional CLI argument: a single path to open at startup. Same effect
//! as drag-and-dropping that file onto the window — exercises the
//! identical Loader → wake → drain → Scintilla pipeline. Useful for
//! Phase 2 demo verification scripts that can't simulate cross-process
//! drag-drop (HGLOBAL is per-process).

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    let initial_path = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    match codepp_ui_win32::run(initial_path) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Code++: fatal error: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "Code++ does not yet have a UI backend for this platform. \
         GTK and Cocoa backends land in Phase 5 — see DESIGN.md §7.2."
    );
    std::process::exit(1);
}
