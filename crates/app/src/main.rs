//! Code++ entry point. Selects the UI backend at compile time.
//!
//! Phase 0: Windows backend opens an empty window; other platforms compile
//! and exit with an unsupported-platform message.

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    match codepp_ui_win32::run() {
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
