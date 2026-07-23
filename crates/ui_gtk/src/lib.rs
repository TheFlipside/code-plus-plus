//! GTK 3 UI backend for Code++.
//!
//! Scope so far: Linux opens, edits, saves and restores a session
//! against real files, with encoding and EOL in the status bar,
//! external changes detected, a working tab strip (switch, close,
//! middle-click-close, drag-to-reorder), and Find/Replace + Goto with
//! a Search menu. The toolbar, UDL styling and the plugin host are
//! later milestones. (Plain code spans, not
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
mod search;
mod state;
mod status;
mod tabs;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk::glib::translate::FromGlibPtrNone;
use gtk::prelude::*;

use codepp_core::perf::Perf;
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
/// `perf` carries the clock `main` started; it is inert unless
/// `--perf` was passed. See `codepp_core::perf` for what is measured
/// and why the clock is not started here.
///
/// # Errors
///
/// Returns [`GtkUiError`] if GTK will not initialise, if Scintilla will
/// not construct its widget, if the direct-call pair cannot be
/// captured, or if `Shell` will not start. All four are fatal setup
/// failures.
pub fn run(initial_path: Option<PathBuf>, perf: Perf) -> Result<(), GtkUiError> {
    // Log the underlying `BoolError` before collapsing it: the
    // `Display` impl on `GtkUiError::GtkInit` names the overwhelmingly
    // likely cause (no display), which would misreport any other
    // failure mode if the real message were discarded entirely.
    gtk::init().map_err(|err| {
        tracing::error!(?err, "gtk::init failed");
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

    // The strip sits above the editor as a sibling, not a parent —
    // its pages are empty and collapse to zero height. See `tabs`.
    let tab_strip = tabs::TabStrip::new();
    layout.pack_start(&tab_strip.notebook, false, false, 0);

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

    let perf = Rc::new(perf);
    connect_perf_probes(&sci_widget, &perf);

    let st = Rc::new(RefCell::new(GtkUiState {
        window: window.clone(),
        sci_widget: sci_widget.clone(),
        editor,
        status,
        menu_bar,
        tabs: tab_strip.clone(),
        shell,
        find_replace: None,
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
        // Resync the strip only when the dirty marker actually flips —
        // twice per edit session (first keystroke after a save, and
        // the save itself), not on every caret move. `sync` rebuilds
        // each tab's label widget, which is far too much to do per
        // notification.
        if refresh_active_dirty() == DirtyPoll::Changed {
            sync_tab_strip();
        }
        None
    });

    connect_tab_strip_signals(&tab_strip);

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
    // After the loop, so the distribution covers the whole session.
    perf.report();
    Ok(())
}

/// Wire the tab strip's two signals to `Shell`.
///
/// Split out of `run` for length, but they belong together anyway:
/// both are guarded by `tabs::is_suppressed`, and the reason is the
/// same for both — see the `tabs` module docs for the GTK 3.24
/// measurements behind that guard.
fn connect_tab_strip_signals(tab_strip: &tabs::TabStrip) {
    //
    // Both handlers bail on `is_suppressed`, because `TabStrip::sync`
    // provokes both of these signals itself while rewriting the
    // notebook — see the `tabs` module docs for the measurements.
    tab_strip.notebook.connect_switch_page(|_, _, num| {
        if tabs::is_suppressed() {
            return;
        }
        // Attribute the outgoing buffer's modify bit to the outgoing
        // tab, while the view is still bound to its document. After
        // `active_tab` moves it would land on the wrong tab.
        capture_active_dirty();
        let moved = with_state(|st| {
            let idx = num as usize;
            if idx < st.shell.tabs.len() {
                st.shell.active_tab = Some(idx);
                true
            } else {
                // The control and the model disagree. Defensive only —
                // `sync` keeps them in lockstep — but silently doing
                // nothing beats binding the view to a tab that is not
                // there.
                tracing::warn!(idx, "switch-page for an index Shell does not have");
                false
            }
        });
        if moved == Some(true) {
            rebind_active_view();
            // No `queue_buffer_activated` here: it is
            // `#[cfg(target_os = "windows")]` in `shell` because it
            // queues the `NPPN_BUFFERACTIVATED` plugin notification,
            // and `platform::dynlib` has no `dlopen` arm yet, so GTK
            // loads no plugins to notify. It joins this handler when
            // the plugin host is ported.
        }
    });

    let strip_for_reorder = tab_strip.clone();
    tab_strip
        .notebook
        .connect_page_reordered(move |_, child, num| {
            if tabs::is_suppressed() {
                return;
            }
            let Some(from) = strip_for_reorder.index_before_reorder(child) else {
                tracing::warn!("page-reordered for a page the strip does not know");
                return;
            };
            // `move_tab` enforces the pinned-prefix invariant and returns
            // false for a drag that would break it. Either way the strip
            // is resynced below: on success it reflects the new order, and
            // on rejection the relabel-by-index puts the visible order
            // back where the model says it should be.
            with_state(|st| st.shell.move_tab(from, num as usize));
            refresh_tab_chrome();
        });
}

/// Attach the DESIGN.md §8 probes to the Scintilla widget.
///
/// All three are no-ops unless `--perf` was passed.
///
/// The interval is opened by a key press and closed by Scintilla's own
/// `SCN_PAINTED`, with `SCN_MODIFIED` in between deciding whether the
/// press counted at all. That middle step is what makes the
/// measurement honest: plenty of keys repaint nothing — Escape,
/// arrows, a Backspace at position 0 — and a press committed without
/// it would wait until some unrelated later paint closed it with a
/// fabricated latency. It is also what lets Tab, Enter and Backspace
/// be measured, which an earlier character-class filter had to exclude
/// wholesale because it could not tell an editing key from an inert
/// one.
///
/// Ctrl chords are skipped: paste, undo, redo and cut all modify the
/// document, but §8 budgets a *typed character*, and a paste's redraw
/// cost is a different quantity that would dominate the tail. `Alt`
/// held alongside `Ctrl` is `AltGr` on many layouts — the way `@`,
/// `{`, `}` and `~` are typed — so those must not be skipped.
///
/// `Propagation::Proceed` on the key handler: it observes, it must
/// never swallow an event.
fn connect_perf_probes(sci_widget: &gtk::Widget, perf: &Rc<Perf>) {
    let perf_key = Rc::clone(perf);
    sci_widget.connect_key_press_event(move |_, ev| {
        let state = ev.state();
        // Ctrl held without Alt is an editing chord. With Alt it is
        // `AltGr` on many layouts, which types real characters.
        let is_chord = state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
            && !state.contains(gtk::gdk::ModifierType::MOD1_MASK);
        if !is_chord {
            perf_key.key_pressed();
        }
        glib::Propagation::Proceed
    });

    // Scintilla reports both remaining edges through `sci-notify`.
    // Using its notifications rather than GTK's `draw` signal matters
    // for the closing edge: `connect_draw` runs *before* Scintilla's
    // own draw handler, so it closed the interval a frame's cairo work
    // early. `SCN_PAINTED` fires when painting is actually done, and
    // is the same notification `ui_win32` uses — so the two platforms
    // now measure the same span rather than approximately the same one.
    let perf_notify = Rc::clone(perf);
    sci_widget.connect_local("sci-notify", false, move |values| {
        match notification_code(values) {
            // `SC_MOD_INSERTTEXT | SC_MOD_DELETETEXT` would be the
            // tighter filter, but reading `modificationType` means
            // depending on the layout of the whole `SCNotification`
            // rather than just its header. Every `SCN_MODIFIED` this
            // backend can receive is a text change today — it sets no
            // margin, annotation or fold-level state — so the code
            // alone is sufficient and the ABI surface stays minimal.
            Some(codepp_scintilla_sys::SCN_MODIFIED) => perf_notify.text_modified(),
            Some(codepp_scintilla_sys::SCN_PAINTED) => {
                perf_notify.mark_first_draw();
                perf_notify.painted();
            }
            _ => {}
        }
        None
    });
}

/// Pull the notification code out of a `sci-notify` emission.
///
/// The signal carries `(ScintillaObject, gint, SCNotification)`, and
/// the payload is a **boxed** type — `g_value_get_pointer` fails on it
/// with a `GLib` critical, which is how the first attempt at this went.
/// `g_value_get_boxed` yields the `SCNotification*`, whose first
/// member is the header this reads.
///
/// Returns `None` rather than guessing if the emission does not have
/// the expected shape.
fn notification_code(values: &[glib::Value]) -> Option<u32> {
    let payload = values.last()?;
    // SAFETY: the value belongs to a `sci-notify` emission, whose
    // payload Scintilla declares as `SCINTILLA_TYPE_NOTIFICATION` —
    // a boxed `SCNotification*`. `g_value_get_boxed` returns that
    // pointer or null; the null case is handled below. Scintilla owns
    // the allocation and it outlives this synchronous handler.
    let header = unsafe {
        glib::gobject_ffi::g_value_get_boxed(payload.as_ptr())
            .cast::<codepp_scintilla_sys::Sci_NotifyHeader>()
    };
    if header.is_null() {
        return None;
    }
    // SAFETY: non-null, and points at a live `SCNotification` for the
    // duration of this handler.
    Some(unsafe { (*header).code })
}

/// Drain everything `Shell` has queued and present any dialogs it
/// returned. Runs on the main thread, from the wake handler.
pub(crate) fn drain_shell() {
    // Frozen for the duration of a tab close (see `close_active_tab`).
    // The close-confirm modal spins a nested main loop that still
    // dispatches the §5.4 wake, and a drain from inside it can remove a
    // tab and shift `active_tab` — `apply_load_result`'s failed-fresh-open
    // branch and `apply_file_change` both do — moving the buffer out from
    // under the user's Save / Don't Save / Cancel decision. It could also
    // stack a second modal via `pump_dialogs`. `close_active_tab` clears
    // the freeze and flushes once, so nothing a worker finished meanwhile
    // is lost; it just lands after the close rather than during it.
    if DrainFreeze::active() {
        return;
    }
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
    refresh_tab_chrome();
}

thread_local! {
    /// Dialogs awaiting presentation. See [`drain_shell`].
    static DIALOG_QUEUE: RefCell<VecDeque<PendingDialog>> =
        const { RefCell::new(VecDeque::new()) };
    /// True while [`pump_dialogs`] owns the queue.
    static PRESENTING: Cell<bool> = const { Cell::new(false) };
    /// Nesting depth of active [`DrainFreeze`] guards. Non-zero while a
    /// close operation is deferring [`drain_shell`]. See [`DrainFreeze`].
    static CLOSE_CONFIRM_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII freeze of [`drain_shell`] for the span of a tab close.
///
/// A close runs a confirm modal that spins a nested main loop; a drain
/// dispatched there could move `active_tab` off the buffer the user is
/// deciding about, or stack a second dialog (see [`drain_shell`]). This
/// guard blocks that for as long as it is held.
///
/// Two properties the bare flag it replaced did not have:
///
///   * **Panic-safe.** The freeze lifts in `Drop`, so a panic inside the
///     confirm handler cannot leave `drain_shell` frozen for the rest of
///     the session (which would silently kill reload prompts and
///     load-completion rebinds). Matters in unwinding debug/test builds;
///     release is `panic = "abort"`.
///   * **Reentrancy-safe.** It is a depth count, not a boolean, so a
///     future Close All that loops `close_active_tab` stays frozen until
///     the *outermost* close finishes rather than the first inner one
///     lifting the freeze early.
struct DrainFreeze;

impl DrainFreeze {
    fn new() -> Self {
        CLOSE_CONFIRM_DEPTH.with(|d| d.set(d.get() + 1));
        Self
    }

    /// Whether any close is currently holding the freeze.
    fn active() -> bool {
        CLOSE_CONFIRM_DEPTH.with(Cell::get) > 0
    }
}

impl Drop for DrainFreeze {
    fn drop(&mut self) {
        CLOSE_CONFIRM_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
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
                // Sanitized: this prompt gates discarding unsaved
                // edits, and `set_secondary_text` renders `\n` as a
                // real line break, so a filename carrying one could
                // forge extra lines that read as part of the official
                // wording. The Win32 sibling (`show_reload_dialog`)
                // does the same; DESIGN.md §7.5 requires them to agree.
                &format!(
                    "{} was modified outside Code++.\n\nReload it?",
                    codepp_shell::sanitize_path_for_display(path)
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
pub(crate) fn message_dialog(
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
                // Fresh opens: each queues an async load whose completion
                // rebinds the view. A duplicate path in session.xml would
                // dedupe to `SwitchedToExisting`, with no load to wake, so
                // rebind now — same as `on_open`.
                if let Some(codepp_shell::OpenFileOutcome::SwitchedToExisting(_)) =
                    with_state(|st| st.shell.open_file(path))
                {
                    rebind_active_view();
                }
            }
            // Re-create an untitled buffer from its backup text, seeded
            // synchronously into a fresh Scintilla document. Mirrors the
            // Win32 restore loop; `Shell::restore_untitled_with_text`
            // does the work and both backends share it.
            SessionRestoreEntry::UntitledFromBackup {
                untitled_seq,
                text,
                cursor,
                encoding,
                eol,
                backup_modified_externally,
                custom_name,
                lang,
                pinned,
            } => {
                with_state(|st| {
                    let (shell, mut ui) = st.split();
                    shell.restore_untitled_with_text(
                        &mut ui,
                        untitled_seq,
                        text,
                        cursor,
                        encoding,
                        eol,
                        backup_modified_externally,
                        custom_name,
                        lang,
                        pinned,
                    );
                });
            }
            // Re-create a path-bound tab whose backup holds the user's
            // last unsaved edits: the tab opens associated with `path`
            // but seeded with the backup text and left dirty, so Save
            // flushes the recovered edits. The two "changed externally"
            // flags route their warnings through `deferred_dialogs`,
            // surfaced by the `drain_shell` at the end of this function.
            SessionRestoreEntry::DirtyFromBackup {
                path,
                text,
                cursor,
                encoding,
                eol,
                disk_changed_externally,
                backup_modified_externally,
                lang,
                pinned,
            } => {
                with_state(|st| {
                    let (shell, mut ui) = st.split();
                    shell.restore_dirty_with_text(
                        &mut ui,
                        path,
                        text,
                        cursor,
                        encoding,
                        eol,
                        disk_changed_externally,
                        backup_modified_externally,
                        lang,
                        pinned,
                    );
                });
            }
        }
    }
    // Restore which tab was in front. Every entry now pushes exactly one
    // tab in session order — `OpenFile` synchronously (its load lands
    // later), the backup variants synchronously with their text — so the
    // persisted active index maps straight across, the same resolution
    // Win32 uses. (This backend previously matched by path because it
    // skipped the backup variants and the resulting index shift was
    // silent; with nothing skipped that workaround is gone, and it also
    // never handled an untitled active tab, which has no path to match.)
    // A rare unreadable-backup drop in `load_session_entries` can still
    // shift it, so bounds-check and fall back to the last-restored tab,
    // exactly as Win32 does.
    if let Some(idx) = with_state(|st| st.shell.session_active_index()).flatten() {
        with_state(|st| {
            if idx < st.shell.tabs.len() {
                st.shell.active_tab = Some(idx);
            } else {
                tracing::warn!(
                    session_active = idx,
                    restored = st.shell.tabs.len(),
                    "session.xml active index out of range; using last-restored tab"
                );
            }
        });
    }

    if let Some(path) = initial_path {
        if let Some(codepp_shell::OpenFileOutcome::SwitchedToExisting(_)) =
            with_state(|st| st.shell.open_file(path))
        {
            rebind_active_view();
        }
    }
    // Nothing restored and nothing named on the command line: start with
    // a fresh untitled buffer, the "new 1" Win32's `ensure_one_tab`
    // creates. Without this the placeholder document bound at startup has
    // no backing `Tab`, so anything typed into it cannot be saved, backed
    // up, or restored — which is exactly the recovery path this milestone
    // is about.
    let has_tabs = with_state(|st| !st.shell.tabs.is_empty()).unwrap_or(false);
    if !has_tabs {
        with_state(|st| {
            let (shell, mut ui) = st.split();
            shell.new_untitled(&mut ui);
        });
    }
    // Loads are asynchronous: the worker threads wake us when the bytes
    // arrive. Drain once now so anything already queued lands before
    // the first paint.
    drain_shell();
    // And bind the view to whichever tab ended up active. `drain_shell`
    // only rebinds for a load that completes *onto the active tab*, so
    // a restore where the active tab's bytes arrived before the index
    // above was applied would otherwise leave the view showing a
    // different buffer than the strip highlights — the exact
    // active-vs-bound split `Shell::bind_active_view` documents as the
    // most damaging state this crate can produce. Idempotent, so
    // running it when the drain already bound correctly costs nothing.
    rebind_active_view();
    reseed_active_caret();
}

/// Restore the active tab's caret to its persisted position after a
/// session restore.
///
/// The trailing `bind_active_view` can perform a real `SCI_SETDOCPOINTER`,
/// and Scintilla clears the caret to 0 on every genuine doc swap — while
/// `bind_active_view`'s existing-doc path never re-applies the stored
/// cursor. `activate_tab`'s same-doc guard avoids the swap only when the
/// active tab is the one already bound (a single-tab restore, or a
/// multi-tab one whose active tab was the last synchronously bound); when
/// an *earlier* backup tab is the active one the swap is unavoidable, so
/// the caret is restored explicitly here. `session.tabs` still holds the
/// loaded cursor (`load_session_entries` kept it). For an `OpenFile`
/// active tab the buffer is still empty now, so this clamps to 0 and
/// `apply_load_result` applies the real cursor when the async load lands —
/// harmless either way. Scintilla clamps an out-of-range value to the
/// document length.
///
/// Only fires when the active tab is still the session's persisted active
/// tab: a command-line file (`initial_path`) moves `active_tab` onto its
/// own freshly-opened tab, whose caret is that file's concern — stamping
/// the session tab's cursor onto it would land the caret at an unrelated
/// position.
fn reseed_active_caret() {
    let cursor = with_state(|st| {
        let active_idx = st.shell.session_active_index()?;
        if st.shell.active_tab != Some(active_idx) {
            return None;
        }
        st.shell.session.tabs.get(active_idx).map(|t| t.cursor)
    })
    .flatten();
    if let Some(cursor) = cursor {
        with_state(|st| {
            st.editor
                .send(codepp_scintilla_sys::SCI_GOTOPOS, cursor as usize, 0);
        });
    }
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
    refresh_tab_chrome();
}

/// Show the "Save file 'NAME' ?" Save / Don't Save / Cancel prompt when
/// the active buffer has unsaved changes, and act on the choice. Returns
/// `true` if the close may proceed (buffer clean, user chose Don't Save,
/// or a requested save succeeded) and `false` if it must be aborted (user
/// chose Cancel, or the save failed / its Save-As chooser was cancelled).
///
/// This mirrors Win32's `handle_close_active_tab_inner` gate. Both GTK
/// close paths funnel through [`close_active_tab`] with the target tab
/// already active (see [`close_tab_by_id`]), so sampling the *active*
/// buffer is correct for a tab-strip close as much as a menu close.
fn confirm_discard_active() -> bool {
    // Sample under a brief borrow, dropped before the modal runs: the
    // dialog spins its own main loop that re-enters our handlers, and a
    // live borrow at that point would make `with_state` decline.
    //
    // The "dirty" test ORs the live `SCI_GETMODIFY` bit against the
    // cached `Tab.dirty` for the same reason Win32 does — an externally
    // removed file flips `Tab.dirty` without the Scintilla doc being
    // touched, and closing then must still guard the only surviving copy.
    let sample = with_state(|st| {
        st.shell.active().map(|tab| {
            let name = codepp_shell::tab_display_name(tab);
            let has_path = tab.path.is_some();
            let has_pending_load = tab.pending_load.is_some();
            let dirty = st.editor.send(codepp_scintilla_sys::SCI_GETMODIFY, 0, 0) != 0 || tab.dirty;
            let length = st.editor.send(codepp_scintilla_sys::SCI_GETLENGTH, 0, 0);
            (name, has_path, has_pending_load, dirty, length)
        })
    });
    let Some(active) = sample else {
        // Borrow unavailable — a re-entrant call we do not expect here.
        // Abort rather than guess at a buffer's dirty state and risk
        // discarding it.
        return false;
    };
    let Some((name, has_path, has_pending_load, dirty, length)) = active else {
        // No active tab: nothing to guard, and the close is a no-op.
        return true;
    };
    // Data-loss safeguard, verbatim from Win32: a tab whose async load is
    // still in flight shows an empty buffer the user has not seen, so its
    // "dirty" bit is a lazy-populate artefact, not an edit — never prompt
    // to write it over the real file. And an untitled buffer the user
    // typed-then-erased reports modified but has nothing to save.
    if !(dirty && !has_pending_load && (has_path || length > 0)) {
        return true;
    }
    match save_confirm_dialog(&name) {
        // Discard the edits and proceed; the doc is freed on close.
        gtk::ResponseType::No => true,
        gtk::ResponseType::Yes => {
            // Save via the same path the Save / Save As menu items use: it
            // saves a titled buffer in place, routes an untitled one
            // through the Save As chooser, and shows its own sanitized
            // error dialog on failure. Then re-read the modify bit — a
            // still-dirty buffer means the save failed or its chooser was
            // cancelled, and the close must abort so nothing is lost.
            crate::menu::on_save();
            matches!(
                with_state(|st| st.editor.send(codepp_scintilla_sys::SCI_GETMODIFY, 0, 0) != 0),
                Some(false)
            )
        }
        // Cancel, the window's close button, or any unexpected response:
        // abort the close and leave the buffer open.
        _ => false,
    }
}

/// The "Save file 'NAME' ?" three-way prompt. Title `Save`, question icon
/// (matching Win32's `MB_ICONQUESTION`), buttons Save / Don't Save /
/// Cancel — the GTK sibling of Win32's [`show_save_confirm_dialog`],
/// wording matched verbatim so muscle memory carries across platforms
/// (DESIGN.md §7.5).
fn save_confirm_dialog(name: &str) -> gtk::ResponseType {
    let parent = with_state(|st| st.window.clone());
    let dialog = gtk::MessageDialog::new(
        parent.as_ref(),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Question,
        gtk::ButtonsType::None,
        // `name` is `tab_display_name` output, already sanitized — the
        // control chars that could forge extra dialog lines are gone.
        &format!("Save file '{name}' ?"),
    );
    // Titlebar caption "Save", matching Win32's `MessageBoxW` title.
    dialog.set_title("Save");
    dialog.add_button("_Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("_Don't Save", gtk::ResponseType::No);
    dialog.add_button("_Save", gtk::ResponseType::Yes);
    dialog.set_default_response(gtk::ResponseType::Yes);
    let response = dialog.run();
    // SAFETY: created here and never handed out — see `message_dialog`.
    unsafe {
        dialog.destroy();
    }
    response
}

/// Close the active tab, releasing its Scintilla document and rebinding
/// the view. Returns `false` if the user aborted at the save prompt
/// (Cancel, or a failed/cancelled save) and the tab stayed open; `true`
/// otherwise. Close All relies on the return value to stop on Cancel.
///
/// The release is not optional: documents are reference-counted, and
/// `SCI_CREATEDOCUMENT` hands back a reference Code++ owns. Dropping the
/// tab without `SCI_RELEASEDOCUMENT` leaks the whole buffer for the rest
/// of the process. Order matters — release *before* rebinding, while the
/// view still holds its own implicit reference, so the document cannot
/// be freed out from under the view mid-call.
pub(crate) fn close_active_tab() -> bool {
    let proceed;
    {
        // Freeze shell drains for the whole close. The data-loss gate
        // below runs a modal that spins a nested main loop; without this a
        // worker wake dispatched there could move `active_tab` off the
        // buffer the user is deciding about (see `drain_shell`). The guard
        // lifts on scope exit — including a panic in the confirm handler —
        // so the flush below always runs unfrozen.
        let _freeze = DrainFreeze::new();

        // Data-loss gate: prompt before discarding unsaved edits. Cancel
        // (or a failed save) aborts the close entirely.
        proceed = confirm_discard_active();
        if proceed {
            let closed_doc = with_state(|st| st.shell.close_active_tab().map(|c| c.scintilla_doc));
            if let Some(Some(doc)) = closed_doc {
                if doc != 0 {
                    with_state(|st| {
                        st.editor
                            .send(codepp_scintilla_sys::SCI_RELEASEDOCUMENT, 0, doc);
                    });
                }
            }
            // With no tabs left the view would still show the closed
            // buffer, so give it a fresh empty document to sit on.
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
                refresh_tab_chrome();
            }
        }
    }

    // Unfrozen now: flush anything a worker completed while the modal held
    // the main loop, applied against the post-close state. If this close
    // was itself nested inside another (a future Close All), an outer
    // freeze is still held and this drain defers to the outer flush.
    drain_shell();
    proceed
}

/// Persist the session. Safe to call repeatedly.
pub(crate) fn save_session_now() {
    with_state(|st| {
        let (shell, mut ui) = st.split();
        if let Err(err) = shell.save_session(&mut ui) {
            tracing::warn!(?err, "session save failed");
        }
    });
}

/// Re-read the active buffer's modify bit into `Tab.dirty`, returning
/// `true` if it changed.
///
/// `Tab.dirty` is a cache the UI maintains — its own doc comment says
/// it "mirrors Scintilla's `SCI_GETMODIFY`" — because the tab strip has
/// to paint a dirty marker for *inactive* tabs too, and reading the
/// live bit for those would need the expensive doc-pointer swap on
/// every repaint.
///
/// Polling `SCI_GETMODIFY` here rather than unpacking the notification
/// is deliberate. Win32 keys off `SCN_SAVEPOINTREACHED` /
/// `SCN_SAVEPOINTLEFT`, but GTK delivers notifications as a boxed
/// `SCNotification` through the `sci-notify` `GObject` signal, so reading
/// the code means declaring the struct's layout and unpacking a
/// `glib::Value` — for an answer `SCI_GETMODIFY` gives authoritatively
/// in one direct call. The savepoint notifications are exactly the ones
/// that flip this bit, so polling on every `sci-notify` is equivalent,
/// and one extra direct call sits inside a handler that already makes
/// several.
///
/// Only the *active* tab is updated: it is the only one whose document
/// is bound to the view. Inactive tabs keep the value captured when
/// they were last active, which is what `capture_active_dirty` on the
/// way out of a tab switch is for.
///
/// Returns `Unavailable` rather than `Unchanged` when it could not
/// look, and the distinction is load-bearing. Scintilla's GTK backend
/// emits `sci-notify` *synchronously* from inside the message that
/// caused it — `ScintillaGTK::NotifyParent` calls `g_signal_emit`
/// directly — so `SCI_SETSAVEPOINT` issued from inside a `with_state`
/// closure re-enters this function while that closure still holds the
/// `RefCell`, and `with_state` correctly declines. Collapsing that into
/// `Unchanged` told the caller the dirty bit had not moved when in fact
/// it had just been cleared by the very save in progress, leaving the
/// tab showing an unsaved-changes marker for a file that was on disk.
fn refresh_active_dirty() -> DirtyPoll {
    with_state(|st| {
        let modified = st.editor.send(codepp_scintilla_sys::SCI_GETMODIFY, 0, 0) != 0;
        let Some(idx) = st.shell.active_tab else {
            return DirtyPoll::Unchanged;
        };
        // Read the id before the mutable borrow below so the
        // `is_unsaved_restore` check (an `&self` method) can run first.
        let Some(id) = st.shell.tabs.get(idx).map(|t| t.id) else {
            return DirtyPoll::Unchanged;
        };
        // A buffer restored from a recovery backup is unsaved even though
        // its document reads clean, so it stays dirty until saved to a
        // real path — otherwise this poll would clear the marker to blue.
        let dirty = modified || st.shell.is_unsaved_restore(id);
        let Some(tab) = st.shell.tabs.get_mut(idx) else {
            return DirtyPoll::Unchanged;
        };
        if tab.dirty == dirty {
            return DirtyPoll::Unchanged;
        }
        tab.dirty = dirty;
        DirtyPoll::Changed
    })
    .unwrap_or(DirtyPoll::Unavailable)
}

/// Outcome of [`refresh_active_dirty`]. See its docs for why
/// "could not look" must not be conflated with "nothing moved".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DirtyPoll {
    /// The cached flag was updated; the strip needs a repaint.
    Changed,
    /// The flag already matched Scintilla. Nothing to do.
    Unchanged,
    /// A re-entrant call could not take the borrow, so the flag was not
    /// examined at all and may now be stale.
    Unavailable,
}

/// Record the outgoing tab's dirty state before a tab switch moves the
/// view off its document.
///
/// Must run *before* `active_tab` changes: it reads the modify bit of
/// whatever document is currently bound, and attributes it to whatever
/// tab is currently active. Reversing the order records the outgoing
/// buffer's dirtiness against the incoming tab.
fn capture_active_dirty() {
    let _ = refresh_active_dirty();
}

/// Close the tab with buffer id `id`, wherever it currently sits.
///
/// Keyed on the id rather than on a captured index because the tab
/// strip's close buttons outlive any particular ordering — see the
/// `tabs` module docs. Activates the tab first, matching Win32, so that
/// a future per-tab save prompt appears against the buffer being
/// closed rather than against whatever was previously in front.
pub(crate) fn close_tab_by_id(id: i32) {
    let Some(Some(idx)) = with_state(|st| st.shell.tabs.iter().position(|t| t.id == id)) else {
        // The tab went away between the label being built and the
        // click landing. Nothing to close, and nothing wrong.
        return;
    };
    let already_active = with_state(|st| st.shell.active_tab == Some(idx)).unwrap_or(false);
    if !already_active {
        capture_active_dirty();
        with_state(|st| st.shell.active_tab = Some(idx));
        rebind_active_view();
    }
    close_active_tab();
}

/// Make the tab strip match `Shell`. Safe and cheap to call often.
pub(crate) fn sync_tab_strip() {
    with_state(|st| {
        let strip = st.tabs.clone();
        strip.sync(&st.shell.tabs, st.shell.active_tab);
    });
}

/// Resync the window title and the tab strip from `Shell`.
///
/// One entry point for both, deliberately: every operation that can
/// change which buffers exist or which one is active has to update
/// both, and having two functions to remember is how a call site ends
/// up updating one and not the other. Named to match `ui_win32`'s
/// `refresh_tab_chrome`, which plays the same role there.
///
/// The title's name comes from `codepp_shell::tab_display_name` rather
/// than from `tab.path` directly, which matters for three separate
/// reasons:
///
///   * **Correctness.** It honours `custom_name` (File → Rename…) and
///     the real `untitled_seq`; the hand-rolled version this replaced
///     ignored the first and hard-coded `"new 1"` for the second, so a
///     renamed buffer and every untitled buffer past the first were
///     both titled wrongly.
///   * **Safety.** It sanitizes. `gtk_window_set_title` takes a C
///     string, so an embedded NUL in a plugin-supplied path — one
///     `NPPM_DOOPEN` away — truncates the title at the NUL in a
///     release build (the window then names a file that is not the one
///     open) and panics inside glib's interior-NUL check in a debug
///     build. Verified against the pinned glib 0.18.5.
///   * **Parity.** DESIGN.md §7.5 requires the backends to agree;
///     `ui_win32`'s `refresh_window_title` resolves the same way.
pub(crate) fn refresh_tab_chrome() {
    with_state(|st| {
        let title = st.shell.active().map_or_else(
            || "Code++".to_string(),
            |tab| format!("{} - Code++", codepp_shell::tab_display_name(tab)),
        );
        st.window.set_title(&title);
    });
    // Re-poll the dirty bit before rendering. Every caller reaches
    // this *after* its own `with_state` block has returned, so unlike
    // the `sci-notify` handler this call is never re-entrant and the
    // borrow is always available. That is what makes the marker
    // correct after a save: the synchronous notification fired from
    // inside `save_current_to_disk`'s borrow was skipped, so without
    // this the strip would repaint from a `Tab.dirty` that still said
    // "modified".
    let _ = refresh_active_dirty();
    sync_tab_strip();
}

/// Guards the single-view model at the source level.
///
/// `EditorHandle` is `Copy` with no lifetime, so a copy outliving its
/// Scintilla widget is not a compile error (see the safety note on
/// `EditorHandle::from_gtk_widget`). What prevents it here is
/// structural: this backend builds exactly one Scintilla widget, never
/// destroys it, and gives tabs their own buffers through
/// `SCI_SETDOCPOINTER` instead of their own views.
///
/// That invariant is a property of the source, so a source check is
/// what can hold it. A runtime test cannot: destroying the view would
/// fault inside vendored C++ on the next direct call rather than fail
/// an assertion, so the failure mode this exists to prevent is exactly
/// the one a runtime test cannot observe.
///
/// DESIGN.md §7.4 carried this as an open ownership question from the
/// Phase 5 m1 security audit until the tab strip landed and settled it.
#[cfg(test)]
mod single_view_source_invariant {
    /// Strip line comments and the contents of string literals, so a
    /// scanner matches code rather than prose. Crude — it does not
    /// handle raw strings or block comments — but the first version of
    /// this guard counted its own assertion text and a doc comment as
    /// real calls, so "crude" needs to at least exclude those.
    fn code_only(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for line in text.lines() {
            let line = line.split("//").next().unwrap_or("");
            let mut in_str = false;
            let mut prev_backslash = false;
            for c in line.chars() {
                match c {
                    '"' if !prev_backslash => in_str = !in_str,
                    _ if in_str => {}
                    _ => out.push(c),
                }
                prev_backslash = c == '\\' && !prev_backslash;
            }
            out.push('\n');
        }
        out
    }

    /// This backend's own source, comments and string literals
    /// removed, tests excluded.
    fn production_code() -> String {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = String::new();
        for entry in std::fs::read_dir(&dir)
            .expect("ui_gtk/src is readable")
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                // Fixtures legitimately build their own widget, so cut
                // at the first test module in each file.
                let cut = text.find("#[cfg(test)]").unwrap_or(text.len());
                out.push_str(&code_only(&text[..cut]));
            }
        }
        out
    }

    #[test]
    fn the_scanner_ignores_comments_and_strings() {
        // The first version of this guard reported three
        // `scintilla_new()` calls where there is one, because it
        // counted a doc comment and its own failure message. Pin that.
        let sample = "\
let a = scintilla_new();
// let b = scintilla_new();
/// `scintilla_new()` returned null
let msg = \"found scintilla_new() calls\";
";
        assert_eq!(code_only(sample).matches("scintilla_new()").count(), 1);
        // And it must not swallow real code that follows a string.
        assert!(code_only("let x = \"a\"; scintilla_new();").contains("scintilla_new()"));
    }

    #[test]
    fn exactly_one_scintilla_widget_is_ever_created() {
        let src = production_code();
        assert!(
            src.len() > 5_000,
            "scanned only {} bytes; the walk is broken, so a clean result proves nothing",
            src.len()
        );
        let calls = src.matches("scintilla_new()").count();
        assert_eq!(
            calls, 1,
            "this backend must build exactly one Scintilla view — found {calls}. \
             A view per tab would leave every copied `EditorHandle` dangling when a \
             tab closes; give tabs their own documents via SCI_SETDOCPOINTER instead."
        );
    }

    #[test]
    fn the_view_is_never_destroyed_or_reassigned() {
        let src = production_code();
        // `let sci_widget = ...` is the one legitimate binding; any
        // other assignment replaces a live view.
        let reassigned = src
            .match_indices("sci_widget =")
            .filter(|(i, _)| !src[..*i].trim_end().ends_with("let"))
            .count();
        assert_eq!(
            reassigned, 0,
            "the Scintilla view is reassigned after creation"
        );
        for forbidden in ["sci_widget.destroy()", "remove(&sci_widget)"] {
            assert!(
                !src.contains(forbidden),
                "found `{forbidden}`: destroying the view dangles every copy of \
                 `EditorHandle`, which is `Copy` and carries no lifetime to stop it"
            );
        }
    }
}
