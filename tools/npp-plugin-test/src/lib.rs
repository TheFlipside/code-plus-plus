//! `npp-plugin-test`: the plugin compatibility harness (DESIGN.md §5.6).
//!
//! It loads a real, unmodified Notepad++ plugin DLL — or several — into
//! Code++'s own plugin host and drives it through the lifecycle the
//! editor drives it through, then reports what the plugin did and what
//! the host answered:
//!
//! 1. **Stage** each DLL into a scratch folder in Notepad++'s layout
//!    (`plugins/<name>/<name>.dll`), so discovery sees exactly the
//!    plugins asked for and the harness itself writes nothing outside
//!    the scratch folder.
//! 2. **Host** them: a real Win32 window is `nppHandle`, two real
//!    Scintilla controls are the views, and the window procedure hands
//!    every `NPPM_*` to `codepp_plugin_host::dispatch_nppm`, the same
//!    dispatcher the editor uses.
//! 3. **Load** through `next_pending_load` / `execute_load` /
//!    `commit_load` — `isUnicode`, `getName`, `setInfo`,
//!    `getFuncsArray` — with no host borrow held, build each plugin's
//!    menu, then deliver `NPPN_TBMODIFICATION`, `NPPN_BUFFERACTIVATED`
//!    and `NPPN_READY` in Notepad++'s order.
//! 4. **Run** any menu commands asked for (`--run`), then **shut
//!    down**: `NPPN_BEFORESHUTDOWN`, `NPPN_SHUTDOWN`, and the host
//!    window destroyed.
//! 5. **Report** each plugin's name and items, every host and Scintilla
//!    message it sent, the messages Code++ does not handle, the panels,
//!    toolbar buttons, status texts and clipboard payloads it offered,
//!    and any dialog that stopped a step. A watchdog closes such a
//!    dialog and ends a step that runs past `--timeout`, so an
//!    unattended run cannot hang.
//!
//! What a plugin asks the *host* to do outside the scratch folder —
//! open a file, save elsewhere, set the clipboard — is recorded, not
//! done. A plugin's own code is not contained: a `--run` command may
//! call Windows directly, as the upstream `NppExport` does to write the
//! clipboard.
//!
//! It is a test of Code++, not of the plugin: a plugin that loads in
//! Notepad++ and fails here is a Code++ bug (CLAUDE.md: breaking
//! Notepad++ binary compatibility is release-blocking). The `unhandled:`
//! lines are the per-message coverage `docs/nppm-coverage.md` asks for.
//!
//! Hosting needs Win32; the report model, the message names, the
//! argument parser and the staging step build and are tested on every
//! platform.

pub mod capture;
pub mod names;
pub mod options;
pub mod report;
pub mod stage;

#[cfg(target_os = "windows")]
mod win;

#[cfg(target_os = "windows")]
pub use win::{run, Run, TIMEOUT_EXIT_CODE};
