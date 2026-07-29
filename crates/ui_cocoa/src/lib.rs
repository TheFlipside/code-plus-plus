//! Cocoa UI backend for Code++.
//!
//! Scope so far (Phase 5 m2): macOS opens a window hosting a real
//! Scintilla view with the DESIGN.md §4.2 direct-call pair captured,
//! wired to `Shell` — so files open and save, the session restores, the
//! 7-part status bar tracks the buffer, external changes are detected,
//! and worker threads wake the UI through the §5.4 marshaling pattern.
//! The tab strip, the toolbar, the dialogs, UDL styling and the plugin
//! host are later milestones. (Plain code spans, not intra-doc links,
//! for cross-crate references in this file — `ui_cocoa` deliberately
//! does not depend on `ui_win32` or `ui_gtk`, so links to them would be
//! unresolvable and would warn on `cargo doc`.)
//!
//! # Why the menus exist this early
//!
//! They are not decoration. AppKit routes ⌘-chords through menu-item key
//! equivalents, so without a menu there is no path from a keypress to
//! `-[SCIContentView undo:]` / `selectAll:` — the methods Scintilla
//! already implements (`cocoa/ScintillaView.mm:913`, `:943`). Typing
//! works without a menu because that arrives through `keyDown:`; ⌘Z and
//! ⌘A do not.
//!
//! # Why no `NSDocument`
//!
//! Same reasoning as `ui_gtk` declining `gtk::Application` and
//! `ui_win32` calling `CreateWindowExW` directly: Code++'s cold-start
//! budget is 80 ms (DESIGN.md §8) and the document architecture's
//! machinery is not on the critical path to the first frame. `Shell`
//! already owns the buffer/session model that `NSDocument` would
//! duplicate.

#![cfg(target_os = "macos")]
// Same rationale as `editor`, `ui_win32` and `ui_gtk` carry: this
// crate's job is translating between Rust types and Scintilla's
// `wparam`/`lparam`/`sptr_t` shapes, so nearly every `as` is a
// deliberate width or sign change whose range is gated by the Scintilla
// ABI (documented in `Scintilla.h`), not by Rust's type system.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

mod delegate;
mod dropview;
mod menu;
mod platform;
mod state;
mod status;
mod tabs;

use std::cell::RefCell;
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use codepp_core::perf::Perf;
use codepp_core::session::WindowGeometry;
use codepp_editor::EditorHandle;
use codepp_scintilla_sys::scintilla_cocoa_new;
use codepp_shell::{PendingDialog, SessionRestoreEntry, Shell};
use dispatch2::DispatchQueue;

use objc2::rc::Retained;
use objc2::MainThreadOnly;
use objc2_app_kit::{
    NSAlert, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSOpenPanel, NSSavePanel, NSScreen, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString, NSURL};

use crate::state::{install, uninstall, with_state, CocoaUiState};
use crate::status::{StatusBar, STATUS_BAR_HEIGHT};
use crate::tabs::{TabStrip, TAB_STRIP_HEIGHT};

/// Initial window size, matching the other two backends' defaults.
const DEFAULT_WIDTH: f64 = 1024.0;
const DEFAULT_HEIGHT: f64 = 768.0;

/// Session auto-save cadence. Same 7 seconds as Win32's `WM_TIMER` arm
/// and GTK's `g_timeout_add_seconds`.
pub(crate) const AUTOSAVE_INTERVAL_SECS: f64 = 7.0;

thread_local! {
    /// Nesting depth of active [`DrainFreeze`] guards. Non-zero while a
    /// modal is up and [`drain_shell`] must not run.
    static MODAL_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The `--perf` recorder, so [`report_perf`] can reach it from the
    /// application delegate's `applicationWillTerminate:`. It cannot
    /// live on `CocoaUiState`, because the report has to survive the
    /// state being torn down in that same handler.
    static PERF: RefCell<Option<Rc<Perf>>> = const { RefCell::new(None) };
}

/// Fatal setup failures. Mirrors `GtkUiError` variant-for-variant where
/// the failure mode exists on both backends.
#[derive(Debug)]
pub enum CocoaUiError {
    /// Not running on the main thread. AppKit is main-thread-only and
    /// `MainThreadMarker::new()` is how that is proven rather than
    /// assumed; this is unreachable through `codepp-app`, whose `main`
    /// calls straight into `run`, but the type system asks for it.
    NotMainThread,
    /// `scintilla_cocoa_new()` returned null — the vendored engine could
    /// not construct its view. Not a user-recoverable state.
    ScintillaCreate,
    /// The view was created but would not surrender its direct-call
    /// `(fn_ptr, instance_ptr)` pair. Continuing would mean routing
    /// every keystroke through a slower fallback path that DESIGN.md
    /// §4.2 forbids, so this is fatal rather than degraded.
    DirectCallCapture,
    /// `Shell::new` failed — most plausibly the file watcher could not
    /// be created.
    Shell(String),
}

impl fmt::Display for CocoaUiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotMainThread => write!(
                f,
                "the Cocoa backend must run on the main thread — AppKit is main-thread-only"
            ),
            Self::ScintillaCreate => write!(f, "scintilla_cocoa_new() returned null"),
            Self::DirectCallCapture => write!(
                f,
                "Scintilla did not return a direct-call function/instance pair"
            ),
            Self::Shell(e) => write!(f, "failed to start the shell: {e}"),
        }
    }
}

impl std::error::Error for CocoaUiError {}

/// Build the window, wire `Shell`, and run the AppKit event loop until
/// the user quits.
///
/// `perf` carries the clock `main` started; it is inert unless `--perf`
/// was passed. See `codepp_core::perf` for what is measured and why the
/// clock is not started here.
///
/// # Errors
///
/// Returns [`CocoaUiError`] if not called on the main thread, if
/// Scintilla will not construct its view, if the direct-call pair
/// cannot be captured, or if `Shell` will not start. All four are fatal
/// setup failures.
// `perf` is taken by value because this signature is a cross-backend
// contract: `codepp-app` has one arm per backend and they must stay
// identical in shape. It is moved into the thread-local below, so it is
// genuinely consumed.
#[allow(clippy::needless_pass_by_value)]
pub fn run(initial_path: Option<PathBuf>, perf: Perf) -> Result<(), CocoaUiError> {
    // AppKit is main-thread-only, and `MainThreadMarker` is objc2's way
    // of proving that once rather than re-asserting it at every call.
    let mtm = MainThreadMarker::new().ok_or(CocoaUiError::NotMainThread)?;

    let app = NSApplication::sharedApplication(mtm);
    // `Regular` gives a Dock icon, a menu bar, and — the part that
    // matters — the ability to become the active application and receive
    // key events. A plain `cargo run` binary is not in an `.app` bundle,
    // so without this the process launches as an accessory and the
    // window never takes focus.
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // The delegate must be installed before the run loop starts: it
    // answers "quit when the last window closes" and owns the only
    // reliable shutdown hook on this platform. See `crate::delegate`.
    let app_delegate = delegate::AppDelegate::install(&app, mtm);

    let actions = menu::Actions::new(mtm);
    let menu = menu::install(&app, &actions, mtm);

    // --- Shell, and the §5.4 cross-thread wake ---------------------
    //
    // Worker threads (the file loader, the watcher) never touch views or
    // Scintilla. They push a typed message onto a channel and call this
    // closure, which hops to the main thread; the main thread then
    // drains the channel and applies the results.
    //
    // `exec_async` takes `FnOnce() + Send`, so the closure must carry no
    // AppKit references — exactly like Win32's
    // `PostMessage(WM_APP_WAKE, 0, 0)`, which carries no payload either,
    // and like GTK's `MainContext::invoke`. It finds the state through a
    // main-thread thread-local once it arrives, the way the Win32
    // wnd_proc recovers its state from `GWLP_USERDATA`.
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {
        DispatchQueue::main().exec_async(|| {
            drain_shell();
        });
    });
    let shell = Shell::new(wake).map_err(|e| CocoaUiError::Shell(e.to_string()))?;

    // --- The Scintilla view ---------------------------------------
    //
    // Created exactly once, and never destroyed, removed or reassigned
    // for the life of the process. Tabs switch buffers underneath it
    // with `SCI_SETDOCPOINTER` rather than getting their own views. That
    // is what makes `EditorHandle`'s raw pointers sound — see
    // `EditorHandle::from_cocoa_view`'s safety documentation, and
    // DESIGN.md §7.4, which names an `NSView`-per-tab design as the
    // specific mistake this avoids.
    //
    // SAFETY: `NSApplication::sharedApplication` has run, which is
    // `scintilla_cocoa_new`'s only precondition beyond being on the main
    // thread — proven above by `mtm`.
    let sci_ptr = unsafe { scintilla_cocoa_new() };
    if sci_ptr.is_null() {
        return Err(CocoaUiError::ScintillaCreate);
    }

    // Adopt the +1 reference the shim handed out, **before** anything
    // that can fail, so every early return below releases it through the
    // normal `Drop` rather than leaking. See the shim's ARC section for
    // why it is `__bridge_retained` on the other side.
    //
    // SAFETY: `sci_ptr` is a non-null, +1-retained `ScintillaView`,
    // which is an `NSView` subclass (`cocoa/ScintillaView.h:79`).
    let sci_view: Retained<NSView> = unsafe { Retained::from_raw(sci_ptr.cast::<NSView>()) }
        .ok_or(CocoaUiError::ScintillaCreate)?;

    // Capture the direct-call pair once, here, per DESIGN.md §4.2 —
    // every hot-path operation from now on bypasses the Objective-C
    // message send entirely. Reads through `sci_ptr` while `sci_view`
    // owns the reference, which is fine: the capture takes no ownership.
    //
    // SAFETY: `sci_ptr` is the non-null view `scintilla_cocoa_new` just
    // returned, still owned by `sci_view` and not yet released.
    let editor =
        unsafe { EditorHandle::from_cocoa_view(sci_ptr) }.ok_or(CocoaUiError::DirectCallCapture)?;

    // --- The window ------------------------------------------------
    let content_rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(DEFAULT_WIDTH, DEFAULT_HEIGHT),
    );
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;

    // SAFETY: standard `NSWindow` designated initialiser; `mtm` proves
    // the main thread, and `defer: false` asks AppKit to create the
    // backing window-server resources now rather than on first display.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content_rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };

    // **Required, not optional.** A programmatically constructed
    // `NSWindow` defaults to `releasedWhenClosed == true`, which makes
    // `-[NSWindow close]` — what the red traffic-light button calls —
    // send an *extra, unbalanced* `release`. That default is designed
    // for callers who hold no strong reference of their own; here
    // `window` is a `Retained<NSWindow>` that does. Leaving the default
    // would let closing the window deallocate it while a live smart
    // pointer still believed it owned a +1.
    //
    // SAFETY: objc2 marks this `unsafe` because setting it to `true` on
    // a window the caller keeps a `Retained` to is what creates the
    // hazard. Passing `false` is the safe direction: it removes the
    // extra release rather than arming it.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("Code++"));

    // Content layout, bottom-up in Cocoa's flipped-origin coordinates:
    // status bar, then the editor, then the tab strip at the top. The
    // editor is the only flexible one. Springs-and-struts rather than
    // Auto Layout — one flexible view between two fixed-height strips is
    // exactly what autoresizing masks express.
    //
    // The content view is a subclass so the whole window accepts dropped
    // files (see `crate::dropview`); Cocoa attaches drag destinations to
    // views, not windows.
    let content = dropview::ContentView::new(content_rect, mtm);
    let status = StatusBar::new(DEFAULT_WIDTH, mtm);
    let tab_strip = TabStrip::new(DEFAULT_WIDTH, mtm);

    let editor_height = DEFAULT_HEIGHT - STATUS_BAR_HEIGHT - TAB_STRIP_HEIGHT;
    sci_view.setFrame(NSRect::new(
        NSPoint::new(0.0, STATUS_BAR_HEIGHT),
        NSSize::new(DEFAULT_WIDTH, editor_height),
    ));
    sci_view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    // The strip sits at the top and stays there as the window grows:
    // width-sizable, with the flexible gap below it.
    tab_strip.container.setFrame(NSRect::new(
        NSPoint::new(0.0, STATUS_BAR_HEIGHT + editor_height),
        NSSize::new(DEFAULT_WIDTH, TAB_STRIP_HEIGHT),
    ));
    tab_strip.container.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
    );

    content.addSubview(&sci_view);
    content.addSubview(&status.container);
    content.addSubview(&tab_strip.container);
    window.setContentView(Some(&content));

    // --- Install the state ----------------------------------------
    let st = Rc::new(RefCell::new(CocoaUiState {
        window: window.clone(),
        sci_view: sci_view.clone(),
        sci_ptr,
        editor,
        status,
        tabs: tab_strip,
        actions: actions.clone(),
        menu,
        shell,
    }));
    install(&st);
    PERF.with(|p| *p.borrow_mut() = Some(Rc::new(perf)));

    // --- Startup work ---------------------------------------------
    apply_startup_styles();
    restore_session(initial_path);
    // MUST run *after* `restore_session` — that is what loads
    // session.xml into the shell, and this reads
    // `saved_window_geometry()` from it. Called earlier it would read an
    // empty session and silently do nothing. Same ordering constraint
    // GTK documents.
    restore_window_geometry(&window, mtm);

    // The auto-save timer is retained by the run loop; `actions` must
    // outlive it, which the binding below guarantees.
    let autosave = actions.start_autosave(mtm);

    // Focus the editor so the first keystroke lands in the buffer.
    window.makeFirstResponder(Some(&sci_view));
    window.makeKeyAndOrderFront(None);
    app.activate();

    // The window is on screen and the event loop is about to take over,
    // which is the closest honest analogue of the other backends' first
    // draw. Recorded before `run()` because `run()` does not return.
    with_state(|s| {
        let (_, ui) = s.split();
        ui.refresh_dynamic_status();
    });
    if let Some(p) = PERF.with(|p| p.borrow().clone()) {
        p.mark_first_draw();
    }

    // Keep the delegate, the action target and the timer alive for the
    // whole session. The reason is the *weak* references:
    // `NSApplication.delegate` and `NSMenuItem.target` are both unowned,
    // so nothing else holds `app_delegate` or `actions`. (`NSTimer` does
    // strongly retain its target, so `actions` is covered twice over —
    // the menu items are the binding constraint.) Binding to a named
    // local rather than `let _ =` is what pins them: `let _ =` drops
    // immediately.
    let _keepalive = (app_delegate, actions, autosave);

    // Blocks until the user quits. Everything after this is unreachable
    // on the standard Quit path — `terminate:` calls `exit()` — which is
    // exactly why session save, the perf report and state teardown live
    // in `applicationWillTerminate:` instead. See `crate::delegate`.
    app.run();

    // Reached only if something stops the run loop without terminating
    // the process — not the standard Quit path, where `terminate:` calls
    // `exit()`. Harmless to repeat: `save_session_now` is idempotent and
    // `uninstall` is a no-op once the state is gone.
    save_session_now();
    report_perf();
    uninstall();
    Ok(())
}

/// RAII freeze of [`drain_shell`] for the span of a modal.
///
/// **This is a correctness guard, not tidiness.** `NSAlert::runModal`
/// and the `NSOpenPanel`/`NSSavePanel` panels spin a nested run loop,
/// and GCD's main-queue source is serviced in that loop — so the §5.4
/// wake fires *during* a modal. A `drain_shell` dispatched there calls
/// `Shell::drain`, and both `apply_load_result`'s failed-fresh-open
/// branch and `apply_file_change` can move `Shell.active_tab`. Save As
/// reads the active tab only *after* its panel returns, so without this
/// freeze a worker completing mid-panel could slide a different buffer
/// under the user's decision and write the wrong file's contents to the
/// path they chose. GTK closed the identical hazard with the same shape
/// (DESIGN.md §7.4); this is the Cocoa port of it.
///
/// Two properties a bare boolean would not have:
///
///   * **Panic-safe.** The freeze lifts in `Drop`, so a panic inside a
///     modal handler cannot leave `drain_shell` frozen for the rest of
///     the session (which would silently kill reload prompts and
///     load-completion rebinds).
///   * **Reentrancy-safe.** It is a depth count, so nested modals stay
///     frozen until the outermost one finishes rather than the first
///     inner one lifting the freeze early.
///
/// Nothing is lost while frozen: the shell's channels are unbounded, so
/// work merely lands after the modal instead of during it. The caller
/// flushes once on release.
struct DrainFreeze;

impl DrainFreeze {
    fn new() -> Self {
        MODAL_DEPTH.with(|d| d.set(d.get() + 1));
        Self
    }

    fn active() -> bool {
        MODAL_DEPTH.with(std::cell::Cell::get) > 0
    }
}

impl Drop for DrainFreeze {
    fn drop(&mut self) {
        MODAL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Emit the `--perf` distribution. Called from the application
/// delegate's `applicationWillTerminate:`, which is the only shutdown
/// hook `terminate:` actually reaches.
pub(crate) fn report_perf() {
    if let Some(p) = PERF.with(|p| p.borrow().clone()) {
        p.report();
    }
}

/// Baseline editor styling, applied once before any buffer exists.
fn apply_startup_styles() {
    with_state(|st| {
        let (_, ui) = st.split();
        platform::apply_predefined_styles(&ui.editor);
    });
}

/// Drain everything worker threads have produced, then resync chrome.
///
/// Reached from the §5.4 wake, hopped onto the main thread by
/// `DispatchQueue::main().exec_async`.
pub(crate) fn drain_shell() {
    // Frozen for the span of any modal — see [`DrainFreeze`]. The work
    // is deferred, not dropped: the channels are unbounded and the
    // modal's caller flushes on release.
    if DrainFreeze::active() {
        return;
    }
    let dialogs = with_state(|st| {
        let (shell, mut ui) = st.split();
        let pending = shell.drain(&mut ui);
        ui.refresh_dynamic_status();
        pending
    });
    // Dialogs are presented after `with_state` has returned: an
    // `NSAlert` runs a modal session that spins its own run loop, and
    // presenting one while the borrow was still live would make every
    // wake behind it a silent no-op (`with_state`'s re-entrancy guard
    // would decline it).
    for dialog in dialogs.unwrap_or_default() {
        present_dialog(dialog);
    }
    refresh_tab_chrome();
}

/// Present one deferred dialog.
///
/// m2 handles the two variants `Shell::drain` can produce without a
/// plugin host; the plugin-driven export dialog arrives with the plugin
/// host itself.
fn present_dialog(dialog: PendingDialog) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // Held for the whole presentation: a wake dispatched into the
    // alert's nested run loop must not drain (and so must not stack a
    // second alert, or move the tab the prompt is about).
    let _freeze = DrainFreeze::new();
    match dialog {
        PendingDialog::ConfirmReload(path) => {
            let name = codepp_shell::sanitize_path_for_display(&path);
            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str(&format!(
                "{name} has been modified by another program."
            )));
            alert.setInformativeText(&NSString::from_str(
                "Reload it from disk? Unsaved changes in this buffer will be lost.",
            ));
            alert.setAlertStyle(NSAlertStyle::Warning);
            alert.addButtonWithTitle(&NSString::from_str("Reload"));
            alert.addButtonWithTitle(&NSString::from_str("Keep Mine"));
            // `NSAlertFirstButtonReturn` is 1000; the first button is
            // "Reload".
            if alert.runModal() == 1000 {
                with_state(|st| st.shell.confirm_reload(path.clone()));
            }
        }
        PendingDialog::Error { title, message } => {
            error_alert(&title, &message);
        }
        // The plugin-driven Save-As export arrives with the plugin
        // host. Named explicitly rather than matched by wildcard: a
        // variant added later should be a compile error here, not a
        // silent log line that nobody reads.
        other @ PendingDialog::SaveExport { .. } => {
            tracing::warn!(?other, "ui_cocoa m2 has no presenter for this dialog yet");
        }
    }
}

/// Show a modal error alert.
fn error_alert(title: &str, message: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let _freeze = DrainFreeze::new();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.setAlertStyle(NSAlertStyle::Critical);
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
}

/// Retitle the window from the active tab's display name.
fn refresh_title() {
    with_state(|st| {
        // `tab_display_name` is the shared, already-sanitized display
        // name — the same helper the other backends title from, so an
        // adversarial filename cannot reach the title bar raw.
        let title = st.shell.active().map_or_else(
            || "Code++".to_string(),
            |t| format!("{} — Code++", codepp_shell::tab_display_name(t)),
        );
        st.window.setTitle(&NSString::from_str(&title));
    });
}

/// Resync the tab strip and the window title from the shell.
///
/// Called after anything that can change the tab list or which tab is
/// active — a drain, a tab switch, a close, an open. Cheap enough to
/// call unconditionally (see `TabStrip::sync` on why it rebuilds
/// wholesale) and never on the keystroke path.
pub(crate) fn refresh_tab_chrome() {
    // Pull the live modified bit into the active tab before painting, so
    // the strip's dirty marker reflects reality. Scintilla's
    // notifications are not wired on this backend yet, so `Tab.dirty`
    // would otherwise stay at whatever the shell last set it to — see
    // `confirm_discard_active` for the same gap and the same fix. The
    // consequence of not having the notification is that the marker
    // updates on model events rather than on the keystroke that first
    // dirties the buffer; wiring `SCN_MODIFIED` is tracked for m3b.
    with_state(|st| {
        let live_dirty = st.editor.send(codepp_scintilla_sys::SCI_GETMODIFY, 0, 0) != 0;
        if let Some(idx) = st.shell.active_tab {
            if let Some(tab) = st.shell.tabs.get_mut(idx) {
                tab.dirty = live_dirty;
            }
        }
    });
    with_state(|st| {
        let mtm = MainThreadMarker::new();
        if let Some(mtm) = mtm {
            let active = st.shell.active_tab;
            // Clone the receiver out first: `sync` borrows the tab list
            // immutably while it reads, and `actions` lives on the same
            // struct.
            let actions = st.actions.clone();
            st.tabs.sync(&st.shell.tabs, active, &actions, mtm);
        }
    });
    refresh_title();
}

/// Rebind the single Scintilla view to whatever tab is now active.
///
/// The counterpart of `ui_gtk::rebind_active_view`. Every tab switch
/// goes through here, because switching tabs on this backend means
/// pointing one view at a different document rather than showing a
/// different view.
pub(crate) fn rebind_active_view() {
    with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.bind_active_view(&mut ui);
    });
    // The single view now holds a different document. Reset the
    // tracking-mode horizontal scroll high-water mark, which is shared
    // across every tab bound to this view and never self-shrinks — so
    // without this, switching from a long-line file to a short one
    // leaves a phantom horizontal scroll. Same fix `ui_gtk` applies.
    with_state(|st| {
        st.editor
            .send(codepp_scintilla_sys::SCI_SETSCROLLWIDTH, 1, 0);
    });
    refresh_tab_chrome();
}

/// Switch the active buffer to the tab with `id`, if it still exists.
///
/// **Id-keyed, not index-keyed.** The strip's buttons outlive any
/// particular ordering, so an index captured when a button was built
/// could address a different buffer by the time it is clicked — DESIGN.md
/// §7.4 records exactly that bug on Win32. Ids are allocated
/// monotonically without reuse, so a stale one resolves to "gone".
pub(crate) fn select_tab_by_id(id: i32) {
    let Some(Some(idx)) = with_state(|st| st.shell.tabs.iter().position(|t| t.id == id)) else {
        return;
    };
    if with_state(|st| st.shell.active_tab == Some(idx)).unwrap_or(false) {
        // Already active — but the click still toggled the button's own
        // push-on/push-off state off, and nothing else would put it
        // back, leaving the current tab painted as unselected. Rebuild
        // the strip so the selection state is re-derived from the model
        // rather than left to AppKit's per-button toggle.
        refresh_tab_chrome();
        return;
    }
    with_state(|st| st.shell.active_tab = Some(idx));
    rebind_active_view();
}

/// Close the tab with `id`, if it still exists.
///
/// Activates it first, so the close path always acts on the active
/// buffer — which is what makes a future save-confirm prompt name the
/// right file. Same shape as `ui_gtk::close_tab_by_id`.
pub(crate) fn close_tab_by_id(id: i32) {
    let Some(Some(idx)) = with_state(|st| st.shell.tabs.iter().position(|t| t.id == id)) else {
        return;
    };
    let already_active = with_state(|st| st.shell.active_tab == Some(idx)).unwrap_or(false);
    if !already_active {
        with_state(|st| st.shell.active_tab = Some(idx));
        rebind_active_view();
    }
    action_close_tab();
}

/// Open a path that was dropped onto the window.
pub(crate) fn open_dropped_path(path: PathBuf) {
    with_state(|st| st.shell.open_file(path));
    refresh_tab_chrome();
}

/// Restore the previous session's buffers, then any path from the
/// command line.
fn restore_session(initial_path: Option<PathBuf>) {
    let entries = with_state(|st| st.shell.load_session_entries()).unwrap_or_default();
    for entry in entries {
        match entry {
            SessionRestoreEntry::OpenFile(path) => {
                // Each queues an async load whose completion rebinds the
                // view through `drain_shell`.
                with_state(|st| st.shell.open_file(path));
            }
            // Re-create an untitled buffer from its backup text, seeded
            // synchronously into a fresh Scintilla document. The shell
            // helper does the work and every backend shares it.
            //
            // Restored in m2 rather than deferred: an untitled buffer
            // with unsaved text exists in no file the user can reopen,
            // so skipping it would leave their work recoverable only by
            // finding the backup by hand. The tab strip is not needed —
            // this function already restores several `OpenFile` tabs
            // without one.
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
            // A path-bound tab whose backup holds the user's last
            // unsaved edits: it opens associated with `path` but seeded
            // with the backup text and left dirty, so Save flushes the
            // recovered edits. The "changed externally" flags route
            // their warnings through `deferred_dialogs`.
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
    // Restore which tab was in front. Every entry above pushes exactly
    // one tab in session order, so the persisted index maps straight
    // across. Done before the command-line path is opened, so an
    // explicitly requested file still ends up active.
    if let Some(Some(idx)) = with_state(|st| st.shell.session_active_index()) {
        with_state(|st| {
            if idx < st.shell.tabs.len() {
                st.shell.active_tab = Some(idx);
            }
        });
    }
    if let Some(path) = initial_path {
        with_state(|st| st.shell.open_file(path));
    }
    // Nothing restored? Give the user an empty buffer to type into,
    // matching what the other backends do.
    let empty = with_state(|st| st.shell.active().is_none()).unwrap_or(true);
    with_state(|st| {
        let (shell, mut ui) = st.split();
        if empty {
            shell.new_untitled(&mut ui);
        } else {
            shell.bind_active_view(&mut ui);
        }
    });
    refresh_tab_chrome();
}

/// Apply the saved window size and position, clamped to a real screen.
///
/// **This function owns window placement outright** — every path either
/// sets an origin or centres. `run` must not call `center()` itself
/// afterwards, or a restored position would be overwritten on every
/// launch (which is exactly the bug the position restore was added to
/// fix).
fn restore_window_geometry(window: &NSWindow, mtm: MainThreadMarker) {
    let Some(Some(geometry)) = with_state(|st| st.shell.saved_window_geometry()) else {
        window.center();
        return;
    };
    let (Some(w), Some(h)) = (geometry.width, geometry.height) else {
        window.center();
        return;
    };
    if w <= 0 || h <= 0 {
        window.center();
        return;
    }
    let mut size = NSSize::new(f64::from(w), f64::from(h));
    // Never restore a window larger than the screen it lands on — a
    // session saved on an external display would otherwise open
    // off-screen on the laptop panel.
    if let Some(screen) = window.screen() {
        let visible = screen.visibleFrame();
        size.width = size.width.min(visible.size.width);
        size.height = size.height.min(visible.size.height);
    }
    window.setContentSize(size);
    // Restore the position too, not just the size. Persisting x/y and
    // then ignoring them would leave the window re-centring every launch
    // while `session.xml` claimed otherwise.
    //
    // Guarded on the origin landing inside some screen's visible frame:
    // a session saved with an external display attached would otherwise
    // place the window off-screen on the laptop panel, with no way to
    // drag it back.
    if let (Some(x), Some(y)) = (geometry.x, geometry.y) {
        let origin = NSPoint::new(f64::from(x), f64::from(y));
        let on_screen = NSScreen::screens(mtm).iter().any(|screen| {
            let f = screen.visibleFrame();
            origin.x >= f.origin.x
                && origin.y >= f.origin.y
                && origin.x < f.origin.x + f.size.width
                && origin.y < f.origin.y + f.size.height
        });
        if on_screen {
            window.setFrameOrigin(origin);
            if geometry.maximized {
                window.zoom(None);
            }
            return;
        }
    }
    window.center();
    if geometry.maximized {
        window.zoom(None);
    }
}

/// Record the window's current size into the shell, so the next save
/// persists it.
fn sync_window_geometry_to_shell() {
    with_state(|st| {
        let frame = st
            .window
            .contentView()
            .map_or_else(|| st.window.frame(), |v| v.frame());
        let maximized = st.window.isZoomed();
        let origin = st.window.frame().origin;
        st.shell.set_window_geometry(WindowGeometry {
            width: Some(frame.size.width as i32),
            height: Some(frame.size.height as i32),
            x: Some(origin.x as i32),
            y: Some(origin.y as i32),
            maximized,
        });
    });
}

/// Persist the session now. Idempotent; safe to call repeatedly.
pub(crate) fn save_session_now() {
    sync_window_geometry_to_shell();
    with_state(|st| {
        let (shell, mut ui) = st.split();
        if let Err(e) = shell.save_session(&mut ui) {
            // Debug sigil: a `ShellError`'s `Display` can embed a path,
            // which is attacker-influenced. Enforced workspace-wide by
            // `codepp_core::tracing_sigil_guard`.
            tracing::warn!(error = ?e, "session save failed");
        }
    });
}

// --- Menu actions --------------------------------------------------
//
// Each is called from `menu::Actions`' Objective-C methods. They take no
// arguments and reach everything through `with_state`, which is what
// keeps the ObjC side trivial.

pub(crate) fn action_new_file() {
    with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.new_untitled(&mut ui);
    });
    refresh_tab_chrome();
}

pub(crate) fn action_open_file() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // See [`DrainFreeze`]: the panel's nested run loop still services
    // the §5.4 wake, and a drain during it can move the active tab.
    let _freeze = DrainFreeze::new();
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(true);
    // `NSModalResponseOK` is 1.
    if panel.runModal() != 1 {
        return;
    }
    for url in panel.URLs() {
        if let Some(path) = url_to_path(&url) {
            with_state(|st| st.shell.open_file(path));
        }
    }
    refresh_tab_chrome();
}

pub(crate) fn action_save_file() {
    // An untitled buffer has no path to save to, so Save behaves as Save
    // As — same as Notepad++. Decided *before* attempting the save, not
    // by treating any error as "must be untitled": a permission-denied
    // or disk-full failure on a titled file has to surface as an error,
    // or the user is left believing their work reached disk when a
    // Save-As panel they cancelled was the only sign anything went
    // wrong. Same shape as `ui_gtk::menu::on_save`.
    let has_path = with_state(|st| st.shell.active().is_some_and(|t| t.path.is_some()));
    if has_path == Some(false) {
        action_save_file_as();
        return;
    }
    let result = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.save_current_to_disk(&mut ui)
    });
    if let Some(Err(e)) = result {
        error_alert(
            "Save failed",
            &codepp_shell::sanitize_str_for_display(&e.to_string()),
        );
    }
    refresh_title();
}

pub(crate) fn action_save_file_as() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // Load-bearing here specifically: `save_buffer_as` reads the active
    // tab *after* the panel returns, so a worker wake landing mid-panel
    // could otherwise write a different buffer's text to the path the
    // user just chose. See [`DrainFreeze`].
    let _freeze = DrainFreeze::new();
    let panel = NSSavePanel::savePanel(mtm);
    if panel.runModal() != 1 {
        return;
    }
    let Some(path) = panel.URL().as_deref().and_then(url_to_path) else {
        return;
    };
    let result = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.save_buffer_as(&mut ui, path)
    });
    if let Some(Err(e)) = result {
        // `ShellError`'s `Display` can carry a path, which is
        // attacker-influenced; an alert renders bidi controls as real
        // dialog text. Same policy the shell applies everywhere else.
        error_alert(
            "Save As failed",
            &codepp_shell::sanitize_str_for_display(&e.to_string()),
        );
    }
    refresh_title();
}

pub(crate) fn action_save_all() {
    let failures = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.save_all(&mut ui)
    })
    .unwrap_or_default();
    if let Some((_, e)) = failures.first() {
        error_alert(
            "Save All failed",
            &codepp_shell::sanitize_str_for_display(&e.to_string()),
        );
    }
    refresh_title();
}

/// Show the Save / Don't Save / Cancel prompt when the active buffer has
/// unsaved changes, and act on the choice.
///
/// Returns `true` if the close may proceed (buffer clean, user chose
/// Don't Save, or a requested save succeeded) and `false` if it must be
/// aborted. Mirrors `ui_gtk::confirm_discard_active` and Win32's
/// `handle_close_active_tab_inner` gate.
fn confirm_discard_active() -> bool {
    // Sample under a brief borrow, dropped before the modal runs: the
    // alert spins its own run loop that re-enters our handlers, and a
    // live borrow at that point would make `with_state` decline.
    let Some(Some((dirty, name))) = with_state(|st| {
        // The **live** `SCI_GETMODIFY` bit, ORed with the cached flag —
        // not the cached flag alone. `Tab.dirty` is only ever written by
        // the shell's crash-recovery restore paths; nothing on this
        // backend sets it in response to typing, because Scintilla's
        // notifications are not wired here yet. Reading it alone made
        // this whole gate inert: every ordinary typed edit reported
        // clean and closed without a prompt. Same OR that
        // `ui_gtk::confirm_discard_active` does, for the same reason.
        let live_dirty = st.editor.send(codepp_scintilla_sys::SCI_GETMODIFY, 0, 0) != 0;
        st.shell.active().map(|t| {
            // Skip the prompt while a load is still in flight (the
            // cached bit is a lazy-populate artifact then, not a real
            // edit), and for an untitled buffer that has been typed into
            // and erased back to empty. Both carried over from GTK.
            let length = st.editor.send(codepp_scintilla_sys::SCI_GETLENGTH, 0, 0);
            let worth_prompting = t.pending_load.is_none() && (t.path.is_some() || length > 0);
            (
                (live_dirty || t.dirty) && worth_prompting,
                codepp_shell::tab_display_name(t),
            )
        })
    }) else {
        return true;
    };
    if !dirty {
        return true;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };

    // Held across the whole prompt. A worker wake dispatched into the
    // alert's nested run loop could otherwise `drain` and move
    // `active_tab`, sliding a different buffer under the user's
    // decision — the exact race DESIGN.md §7.4 records for GTK's close
    // path. See [`DrainFreeze`]. Named without an underscore because the
    // Save arm drops it explicitly before re-entering `action_save_file`,
    // which runs its own modal.
    let freeze = DrainFreeze::new();

    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!("Save changes to {name}?")));
    alert.setInformativeText(&NSString::from_str(
        "Your changes will be lost if you don't save them.",
    ));
    alert.setAlertStyle(NSAlertStyle::Warning);
    // Button order is macOS convention — the default action first, the
    // destructive one last — which is the reverse of the Win32 dialog's
    // reading order but the right thing on this platform.
    alert.addButtonWithTitle(&NSString::from_str("Save"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.addButtonWithTitle(&NSString::from_str("Don't Save"));

    // `NSAlertFirstButtonReturn` is 1000; the rest count up.
    match alert.runModal() {
        1000 => {
            // Save, and only proceed if it actually succeeded — a
            // failed save that still closed the tab is precisely the
            // data loss this prompt exists to prevent. The freeze is
            // released first because `action_save_file` may open its own
            // Save-As panel, which takes the freeze again; holding both
            // would work (it is a depth count) but releasing here keeps
            // the nesting shallow and the intent obvious.
            drop(freeze);
            action_save_file();
            with_state(|st| st.shell.active().is_some_and(|t| !t.dirty)).unwrap_or(false)
        }
        1002 => true, // Don't Save
        _ => false,   // Cancel, or the window closed under the alert
    }
}

/// Close the active tab, gated on the unsaved-changes prompt.
pub(crate) fn action_close_tab() {
    if confirm_discard_active() {
        // Release the closed buffer's Scintilla document. `ClosedTab`
        // hands back the doc pointer precisely so the UI can do this —
        // dropping the tab without `SCI_RELEASEDOCUMENT` leaks the whole
        // buffer for the rest of the process, because the single view no
        // longer references it and Scintilla refcounts documents.
        let closed_doc = with_state(|st| st.shell.close_active_tab().map(|c| c.scintilla_doc));
        if let Some(Some(doc)) = closed_doc {
            if doc != 0 {
                with_state(|st| {
                    st.editor
                        .send(codepp_scintilla_sys::SCI_RELEASEDOCUMENT, 0, doc);
                });
            }
        }
        // Closing the *last* tab must leave a fresh untitled buffer, not
        // a tab-less placeholder. A placeholder with no backing `Tab` is
        // the "null" state: the strip paints from `shell.tabs` and so
        // collapses to nothing, while the view still shows the closed
        // document — typing into it and pressing ⌘W again would discard
        // the edits with no prompt, because there is no `Tab` for the
        // gate above to find. `new_untitled` creates a real, tracked,
        // saveable tab. The Cocoa equivalent of Win32's `ensure_one_tab`
        // and GTK's same fallback. (No leak: the new doc belongs to the
        // new `Tab` and is released when that tab is closed.)
        let has_active = with_state(|st| st.shell.active_tab.is_some()).unwrap_or(false);
        if !has_active {
            with_state(|st| {
                let (shell, mut ui) = st.split();
                shell.new_untitled(&mut ui);
            });
        }
        rebind_active_view();
    }
    // Unfrozen now: flush whatever a worker completed while the modal
    // held the run loop, applied against the post-close state.
    // **Unconditional**, including on Cancel: a wake dispatched during
    // the prompt fired and no-op'd rather than queuing a retry, so
    // without this flush a pending dialog for an unrelated tab could sit
    // unpresented indefinitely.
    drain_shell();
}

pub(crate) fn action_reload() {
    with_state(|st| st.shell.reload_active());
}

/// Convert an `NSURL` from a panel into a filesystem path.
fn url_to_path(url: &NSURL) -> Option<PathBuf> {
    url.path().map(|p| PathBuf::from(p.to_string()))
}
