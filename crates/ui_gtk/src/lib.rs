//! GTK 3 UI backend for Code++.
//!
//! Phase 5 m2 scope: bring Linux to Phase 2 parity — open, edit, save
//! and restore a session against real files, with encoding and EOL
//! shown in the status bar and external changes detected. The tab
//! strip, Find/Replace and Goto dialogs, the toolbar, UDL styling and
//! the plugin host are later milestones. (Plain code spans, not
//! intra-doc links, for cross-crate references in this file — `ui_gtk`
//! deliberately does not depend on `ui_win32`, so links to it would be
//! unresolvable and would warn on `cargo doc`.)
//!
//! # Why GTK 3
//!
//! Scintilla ships no GTK 4 backend — see
//! `crates/scintilla-sys/build.rs::build_scintilla_gtk` for the
//! evidence and DESIGN.md §4.1 for the amended decision. GTK 3.24 is
//! the final, API-frozen GTK 3 series, so this is a stable target.
//!
//! # Why no `gtk::Application`
//!
//! [`gtk::Application`] wraps `GApplication`, which registers on the
//! session D-Bus at startup and performs single-instance arbitration.
//! Code++'s cold-start budget is 80 ms (DESIGN.md §8) and none of that
//! machinery is on the critical path to the first frame, so this
//! backend uses the lower-level `gtk::init` + `gtk::main` pair instead.
//! Same reasoning as `ui_win32` calling `CreateWindowExW` directly
//! rather than adopting a framework.

#![cfg(target_os = "linux")]
// Same rationale as `editor` and `ui_win32` carry: this crate's job is
// translating between Rust types and Scintilla's `wparam`/`lparam`/
// `sptr_t` shapes, so nearly every `as` is a deliberate width or sign
// change whose range is gated by the Scintilla ABI (documented in
// `Scintilla.h`), not by Rust's type system. Attributing each of the
// ~18 sites individually would add more noise than defence.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

mod menu;
mod platform;
mod state;
mod status;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk::glib::translate::FromGlibPtrNone;
use gtk::prelude::*;

use codepp_editor::EditorHandle;
use codepp_scintilla_sys::scintilla_new;
use codepp_shell::{PendingDialog, SessionRestoreEntry, Shell};

use state::with_state;
use state::GtkUiState;
use status::StatusBar;

/// Initial window size, in logical pixels. GTK scales this for `HiDPI`.
const DEFAULT_WIDTH: i32 = 1024;
/// See [`DEFAULT_WIDTH`].
const DEFAULT_HEIGHT: i32 = 768;

/// Session auto-save cadence. Matches the Win32 backend's `SetTimer`
/// interval so a crash loses at most the same amount of work on either
/// platform (DESIGN.md §7.5).
const AUTOSAVE_INTERVAL_SECS: u32 = 7;

/// Everything that can go wrong bringing the GTK backend up.
///
/// Deliberately small: each variant is a hard setup failure with no
/// recovery path, so the binary reports it and exits non-zero.
#[derive(Debug)]
pub enum GtkUiError {
    /// `gtk_init_check` failed — almost always "no display available"
    /// (running headless, or `DISPLAY`/`WAYLAND_DISPLAY` unset).
    GtkInit,
    /// `scintilla_new()` returned null. Means the vendored engine
    /// failed to construct its widget; not a user-recoverable state.
    ScintillaCreate,
    /// The widget was created but would not surrender its direct-call
    /// `(fn_ptr, instance_ptr)` pair. Continuing would mean routing
    /// every keystroke through a slower fallback path that DESIGN.md
    /// §4.2 forbids, so this is fatal rather than degraded.
    DirectCallCapture,
    /// `Shell::new` failed — most plausibly the file watcher could not
    /// be created (inotify instance limit reached).
    Shell(String),
}

impl fmt::Display for GtkUiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GtkInit => write!(
                f,
                "failed to initialise GTK — is a display available? \
                 (DISPLAY / WAYLAND_DISPLAY unset when running headless)"
            ),
            Self::ScintillaCreate => write!(f, "scintilla_new() returned null"),
            Self::DirectCallCapture => write!(
                f,
                "Scintilla did not return a direct-call function/instance pair"
            ),
            Self::Shell(e) => write!(f, "failed to start the shell: {e}"),
        }
    }
}

impl std::error::Error for GtkUiError {}

/// Build the window, wire `Shell`, and run the GTK main loop until the
/// user closes the window.
///
/// # Errors
///
/// Returns [`GtkUiError`] if GTK will not initialise, if Scintilla will
/// not construct its widget, if the direct-call pair cannot be
/// captured, or if `Shell` will not start. All four are fatal setup
/// failures.
pub fn run(initial_path: Option<PathBuf>) -> Result<(), GtkUiError> {
    // Log the underlying `BoolError` before collapsing it: the
    // `Display` impl on `GtkUiError::GtkInit` names the overwhelmingly
    // likely cause (no display), which would misreport any other
    // failure mode if the real message were discarded entirely.
    gtk::init().map_err(|err| {
        tracing::error!(%err, "gtk::init failed");
        GtkUiError::GtkInit
    })?;

    // --- Shell, and the §5.4 cross-thread wake --------------------
    //
    // Worker threads (the file loader, the watcher, find-in-files)
    // never touch widgets or Scintilla. They push a typed message onto
    // a channel and call this closure, which hops to the main thread;
    // the main thread then drains the channel and applies the results.
    //
    // `MainContext::invoke` takes `FnOnce() + Send`, so the closure
    // must carry no widget references — exactly like Win32's
    // `PostMessage(WM_APP_WAKE, 0, 0)`, which carries no payload
    // either. It finds the state through a main-thread thread-local
    // once it arrives, the way the Win32 wnd_proc recovers its state
    // from `GWLP_USERDATA`.
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {
        glib::MainContext::default().invoke(|| {
            drain_shell();
        });
    });
    let shell = Shell::new(wake).map_err(|e| GtkUiError::Shell(e.to_string()))?;

    // --- Widgets ---------------------------------------------------
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Code++");
    window.set_default_size(DEFAULT_WIDTH, DEFAULT_HEIGHT);

    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&layout);

    let menu_bar = menu::build();
    layout.pack_start(&menu_bar, false, false, 0);

    // SAFETY: `gtk::init` succeeded above, which is `scintilla_new`'s
    // only precondition.
    let sci_ptr = unsafe { scintilla_new() };
    if sci_ptr.is_null() {
        return Err(GtkUiError::ScintillaCreate);
    }
    // Adopt the raw `GtkWidget*` into gtk-rs. `from_glib_none` is the
    // correct transfer mode for a `*_new()` constructor: the pointer
    // carries a floating reference, and `from_glib_none` sinks it —
    // the same call every gtk-rs widget constructor makes when
    // wrapping its own C `gtk_*_new()`.
    //
    // SAFETY: `sci_ptr` is a non-null widget that `scintilla_new` just
    // returned and nothing has unreffed since.
    let sci_widget = unsafe { gtk::Widget::from_glib_none(sci_ptr.cast::<gtk::ffi::GtkWidget>()) };
    layout.pack_start(&sci_widget, true, true, 0);

    let status = StatusBar::new();
    layout.pack_start(&status.container, false, false, 0);

    // Capture the direct-call pair once, here, per DESIGN.md §4.2 —
    // every hot-path operation from now on bypasses GTK entirely.
    //
    // SAFETY: `sci_ptr` is the live widget just packed into `layout`,
    // which holds a reference to it for the rest of the process.
    let editor =
        unsafe { EditorHandle::from_gtk_widget(sci_ptr) }.ok_or(GtkUiError::DirectCallCapture)?;

    let st = Rc::new(RefCell::new(GtkUiState {
        window: window.clone(),
        sci_widget: sci_widget.clone(),
        editor,
        status,
        menu_bar,
        shell,
    }));
    state::install(&st);

    // Scintilla reports caret moves, edits and save-point transitions
    // through its own `sci-notify` GObject signal (declared in
    // `ScintillaWidget.h` as `SCINTILLA_NOTIFY`), which is the GTK
    // analogue of Win32's `WM_NOTIFY`. The payload is ignored on
    // purpose: m2 only needs "something changed, resync the chrome",
    // and refreshing unconditionally is both simpler and cheaper than
    // unpacking `SCNotification` — the refresh is a handful of
    // direct-calls plus label writes GTK elides when the text is
    // unchanged, so it stays far inside the §8 keystroke budget.
    // Milestones that need to distinguish notification codes (UDL
    // container styling needs `SCN_STYLENEEDED`) will unpack it then.
    sci_widget.connect_local("sci-notify", false, |_| {
        with_state(|st| {
            let (_, ui) = st.split();
            ui.refresh_dynamic_status();
        });
        None
    });

    // --- Startup work ---------------------------------------------
    restore_window_geometry(&window);
    apply_startup_styles();
    menu::connect();
    restore_session(initial_path);

    window.connect_delete_event(|_, _| {
        // Persist before tearing down: `Shell::save_session` needs the
        // editor alive to read the caret position back out.
        save_session_now();
        gtk::main_quit();
        glib::Propagation::Proceed
    });

    // Periodic session auto-save. Win32 uses SetTimer + WM_TIMER;
    // `timeout_add_seconds_local` is the direct GTK analogue and stays
    // on the main thread, so it can touch the editor safely.
    glib::timeout_add_seconds_local(AUTOSAVE_INTERVAL_SECS, || {
        save_session_now();
        glib::ControlFlow::Continue
    });

    window.show_all();
    // Focus the editor so the first keystroke lands in the buffer
    // rather than on the menu bar.
    sci_widget.grab_focus();

    gtk::main();

    // Drop the state explicitly so `Shell` — and the worker threads its
    // channels keep alive — tear down here rather than at process exit.
    state::uninstall();
    Ok(())
}

/// Drain everything `Shell` has queued and present any dialogs it
/// returned. Runs on the main thread, from the wake handler.
pub(crate) fn drain_shell() {
    let dialogs = with_state(|st| {
        let (shell, mut ui) = st.split();
        let pending = shell.drain(&mut ui);
        ui.refresh_dynamic_status();
        pending
    });
    // Dialogs are queued rather than presented inline, for two reasons.
    // First, `with_state` has already returned by here: a modal dialog
    // spins its own main loop, and presenting one while the borrow was
    // still live would make every wake behind it a silent no-op.
    // Second, that nested loop still dispatches idle sources, so a
    // worker's wake *during* a dialog re-enters `drain_shell` — without
    // a queue that would open a second dialog on top of the first, and
    // a burst of external changes across many open tabs (a `git
    // checkout`, say) would stack them arbitrarily deep.
    DIALOG_QUEUE.with(|q| q.borrow_mut().extend(dialogs.unwrap_or_default()));
    pump_dialogs();
    refresh_title();
}

thread_local! {
    /// Dialogs awaiting presentation. See [`drain_shell`].
    static DIALOG_QUEUE: RefCell<VecDeque<PendingDialog>> =
        const { RefCell::new(VecDeque::new()) };
    /// True while [`pump_dialogs`] owns the queue.
    static PRESENTING: Cell<bool> = const { Cell::new(false) };
}

/// Present queued dialogs one at a time, never nesting.
///
/// Re-entrant calls return immediately: the outer pump still owns the
/// queue and will pick up anything added while it was blocked in
/// `dialog.run()`. That bounds dialog nesting at one regardless of how
/// many wakes arrive during a modal loop.
fn pump_dialogs() {
    if PRESENTING.with(Cell::get) {
        return;
    }
    PRESENTING.with(|p| p.set(true));
    while let Some(dialog) = DIALOG_QUEUE.with(|q| q.borrow_mut().pop_front()) {
        present_dialog(&dialog);
    }
    PRESENTING.with(|p| p.set(false));
}

/// Map a [`PendingDialog`] onto a native GTK dialog.
fn present_dialog(dialog: &PendingDialog) {
    match dialog {
        PendingDialog::ConfirmReload(path) => {
            let response = message_dialog(
                gtk::MessageType::Question,
                gtk::ButtonsType::YesNo,
                "File changed on disk",
                &format!(
                    "{} was modified outside Code++.\n\nReload it?",
                    path.display()
                ),
            );
            if response == gtk::ResponseType::Yes {
                // No drain needed here: `reload_active` queues a load
                // through the same worker every other open uses, and
                // its wake drains and rebinds the view. Draining
                // inline would just recurse for nothing.
                with_state(|st| st.shell.reload_active());
            }
        }
        PendingDialog::Error { title, message } => {
            message_dialog(
                gtk::MessageType::Error,
                gtk::ButtonsType::Ok,
                title,
                message,
            );
        }
    }
}

/// Run a modal message dialog and return the user's response.
fn message_dialog(
    kind: gtk::MessageType,
    buttons: gtk::ButtonsType,
    title: &str,
    body: &str,
) -> gtk::ResponseType {
    // Parent to the main window when it is reachable so the dialog is
    // centred on it and stays above it. `with_state` returns `None` if
    // a borrow is already live, in which case an unparented dialog is
    // better than no dialog at all.
    let parent = with_state(|st| st.window.clone());
    let dialog = gtk::MessageDialog::new(
        parent.as_ref(),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        kind,
        buttons,
        title,
    );
    dialog.set_secondary_text(Some(body));
    let response = dialog.run();
    // SAFETY: `destroy` is `unsafe` in gtk-rs only because destroying a
    // widget other code still holds invalidates it. This dialog was
    // created three lines up and never handed out.
    unsafe {
        dialog.destroy();
    }
    response
}

/// Apply `styles.xml` to the fresh editor, matching what the Win32
/// backend does right after `Shell::new`.
fn apply_startup_styles() {
    with_state(|st| {
        let styles = st.shell.styles.clone();
        let (_, mut ui) = st.split();
        codepp_shell::UiPlatform::apply_default_style(&mut ui, &styles);
    });
}

/// Restore the window size the last session ended with.
fn restore_window_geometry(window: &gtk::Window) {
    let Some(Some(g)) = with_state(|st| st.shell.saved_window_geometry()) else {
        return;
    };
    if let (Some(w), Some(h)) = (g.width, g.height) {
        if w > 0 && h > 0 {
            window.set_default_size(w, h);
        }
    }
    if g.maximized {
        window.maximize();
    }
}

/// Reopen last session's files, then the command-line path if given.
///
/// A path on the command line is opened *after* the session so it ends
/// up active — the same precedence Win32 uses, and what a user typing
/// `codepp file.txt` expects.
fn restore_session(initial_path: Option<PathBuf>) {
    let entries = with_state(|st| st.shell.load_session_entries()).unwrap_or_default();
    for entry in entries {
        match entry {
            SessionRestoreEntry::OpenFile(path) => {
                // Restores are all fresh opens, so each queues a load
                // whose completion rebinds the view. A duplicate path
                // in session.xml would dedupe to `SwitchedToExisting`
                // though, so handle that the same way `on_open` does.
                if let Some(codepp_shell::OpenFileOutcome::SwitchedToExisting(_)) =
                    with_state(|st| st.shell.open_file(path))
                {
                    rebind_active_view();
                }
            }
            // The backup-restore variants need the dirty-buffer
            // plumbing that arrives with the tab strip. Opening the
            // on-disk file instead would silently discard the user's
            // unsaved work, so skip and say so rather than doing the
            // wrong thing quietly.
            // `SessionRestoreEntry` is not `Debug`, so name the case
            // rather than formatting the value.
            _ => {
                tracing::warn!(
                    "session entry needs dirty-buffer restore, which GTK gains with the \
                     tab strip; skipped rather than silently discarding unsaved work"
                );
            }
        }
    }
    if let Some(path) = initial_path {
        if let Some(codepp_shell::OpenFileOutcome::SwitchedToExisting(_)) =
            with_state(|st| st.shell.open_file(path))
        {
            rebind_active_view();
        }
    }
    // Loads are asynchronous: the worker threads wake us when the bytes
    // arrive. Drain once now so anything already queued lands before
    // the first paint.
    drain_shell();
}

/// Bind the view to `Shell`'s active tab and retitle the window.
///
/// The binding itself lives in `Shell::bind_active_view` so both
/// backends share one implementation — see its docs for why the two
/// must never disagree. This wrapper exists only to supply the
/// `(shell, ui)` split and refresh the title afterwards.
///
/// **Every `Shell` call that can move `active_tab` without queuing a
/// load must be followed by this.** `Shell::drain` only rebinds when a
/// load *completes*, so the synchronous outcomes — `close_active_tab`,
/// and `open_file` returning `SwitchedToExisting` — leave the view on
/// the previous tab's document unless the UI rebinds itself.
fn rebind_active_view() {
    with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.bind_active_view(&mut ui);
    });
    refresh_title();
}

/// Close the active tab, releasing its Scintilla document and rebinding
/// the view.
///
/// The release is not optional: documents are reference-counted, and
/// `SCI_CREATEDOCUMENT` hands back a reference Code++ owns. Dropping the
/// tab without `SCI_RELEASEDOCUMENT` leaks the whole buffer for the rest
/// of the process. Order matters — release *before* rebinding, while the
/// view still holds its own implicit reference, so the document cannot
/// be freed out from under the view mid-call.
pub(crate) fn close_active_tab() {
    let closed_doc = with_state(|st| st.shell.close_active_tab().map(|c| c.scintilla_doc));
    if let Some(Some(doc)) = closed_doc {
        if doc != 0 {
            with_state(|st| {
                st.editor
                    .send(codepp_scintilla_sys::SCI_RELEASEDOCUMENT, 0, doc);
            });
        }
    }
    // With no tabs left the view would still show the closed buffer,
    // so give it a fresh empty document to sit on.
    let has_active = with_state(|st| st.shell.active_tab.is_some()).unwrap_or(false);
    if has_active {
        rebind_active_view();
    } else {
        with_state(|st| {
            let (_, mut ui) = st.split();
            let placeholder = codepp_shell::UiPlatform::activate_tab(&mut ui, 0, 0);
            codepp_shell::UiPlatform::set_buffer_text(&mut ui, "", 0);
            // Release immediately, unlike every other freshly-created
            // document. Elsewhere the new pointer is written onto a
            // `Tab.scintilla_doc`, and that tab owns Code++'s reference
            // until it is itself closed through this function. This
            // placeholder has no tab — `shell.tabs` is empty by
            // construction in this branch — so nothing would ever
            // release it, and closing the last tab would leak a
            // document for the rest of the process.
            //
            // Refcount walk, matching `ui_win32`'s placeholder path:
            // CREATEDOCUMENT gives 1 (ours), SETDOCPOINTER makes it 2
            // (view AddRefs), RELEASEDOCUMENT here drops it back to 1 —
            // just the view's implicit reference. The next
            // SETDOCPOINTER, from a future open's `activate_tab`, drops
            // that last one and frees it cleanly.
            //
            // Guarded on non-zero: `SCI_CREATEDOCUMENT` returns 0 on
            // allocation failure, and releasing null is not part of
            // Scintilla's published ABI contract.
            if placeholder != 0 {
                st.editor
                    .send(codepp_scintilla_sys::SCI_RELEASEDOCUMENT, 0, placeholder);
            }
        });
        refresh_title();
    }
}

/// Persist the session. Safe to call repeatedly.
pub(crate) fn save_session_now() {
    with_state(|st| {
        let (shell, mut ui) = st.split();
        if let Err(err) = shell.save_session(&mut ui) {
            tracing::warn!(%err, "session save failed");
        }
    });
}

/// Retitle the window from the active tab, Notepad++ style.
pub(crate) fn refresh_title() {
    with_state(|st| {
        let title = st.shell.active().map_or_else(
            || "Code++".to_string(),
            |tab| {
                let name = tab
                    .path
                    .as_deref()
                    .and_then(|p| p.file_name())
                    .map_or_else(|| "new 1".to_string(), |n| n.to_string_lossy().into_owned());
                format!("{name} - Code++")
            },
        );
        st.window.set_title(&title);
    });
}
