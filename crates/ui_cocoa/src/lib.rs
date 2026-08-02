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
mod docmap;
mod dropview;
mod fif;
mod menu;
mod platform;
mod preferences;
mod search;
mod state;
mod status;
mod tabs;
mod toolbar;
mod udl;
mod window;
mod workspace;

use std::cell::{Cell, RefCell};
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use codepp_core::perf::Perf;
use codepp_core::preferences::RecentFilesHistoryConfig;
use codepp_core::session::WindowGeometry;
use codepp_editor::EditorHandle;
use codepp_scintilla_sys::scintilla_cocoa_new;
use codepp_shell::{PendingDialog, SessionRestoreEntry, Shell, UiPlatform};
use dispatch2::DispatchQueue;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::Message as _;
use objc2_app_kit::{
    NSAlert, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions,
    NSColorSpace, NSControlSize, NSEvent, NSEventMask, NSEventModifierFlags, NSOpenPanel,
    NSSavePanel, NSScreen, NSScroller, NSScrollerStyle, NSView, NSWindow, NSWindowStyleMask,
    NSWindowTabbingMode,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString, NSURL};

use crate::menu::Actions;
use crate::state::{install, uninstall, with_state, CocoaUiState};
use crate::status::{StatusBar, STATUS_BAR_HEIGHT};
use crate::tabs::{TabStrip, TAB_STRIP_HEIGHT};
use crate::toolbar::{Toolbar, TOOLBAR_HEIGHT};

/// Initial window size, matching the other two backends' defaults.
const DEFAULT_WIDTH: f64 = 1024.0;
const DEFAULT_HEIGHT: f64 = 768.0;

/// Session auto-save cadence. Same 7 seconds as Win32's `WM_TIMER` arm
/// and GTK's `g_timeout_add_seconds`.
pub(crate) const AUTOSAVE_INTERVAL_SECS: f64 = 7.0;

thread_local! {
    /// Viewport width the horizontal floor was last computed against, so
    /// [`clamp_scroll_width_to_viewport`] can tell a resize from a
    /// repaint. `-1` is unreachable, so the first paint always re-seeds.
    static LAST_VIEWPORT_WIDTH: std::cell::Cell<isize> = const { std::cell::Cell::new(-1) };
    /// The buffer the tab strip was last scrolled to show, so an
    /// ordinary chrome refresh does not undo the overflow arrows. See
    /// [`refresh_tab_chrome`].
    static LAST_SCROLLED_TO: Cell<Option<i32>> = const { Cell::new(None) };
    /// The document the horizontal scroll range was last seeded for, so
    /// the seed fires when the view comes to rest on a different buffer
    /// and not on the bookkeeping swaps `Shell::save_all` makes. `0` is
    /// Scintilla's "no document" value and is never a live pointer, so
    /// the first real binding always seeds. See
    /// [`seed_horizontal_scroll_if_document_changed`].
    ///
    /// **This is a raw pointer used as an identity.** A freed document
    /// replaced by a new one at the same address would make the
    /// comparison read "unchanged" for a buffer that had in fact been
    /// swapped — classic ABA. What rules it out is that **an incoming
    /// document is always allocated before the call that could free the
    /// outgoing one**, so the address handed to a bind is never one that
    /// same bind is about to free. `SCI_CREATEDOCUMENT` runs in
    /// `activate_tab` ahead of the swap, and `action_close_tab`'s
    /// `SCI_RELEASEDOCUMENT` runs while the document is still bound, so
    /// it only drops the refcount 2→1 and never frees.
    ///
    /// Note this is *not* an ordering guarantee inside
    /// `Editor::SetDocPointer` — that releases the outgoing document
    /// **before** assigning the incoming one (`Editor.cxx:5451`), and
    /// `Document::Release` deletes synchronously at refcount zero. An
    /// earlier version of this comment had that backwards. The safety
    /// comes from the caller side, not from Scintilla's ordering.
    ///
    /// **Keyed on the document rather than on `Tab.id`, which is the
    /// obvious alternative and does not work.** `Shell::open_file` moves
    /// `active_tab` at request time, before the load has produced a
    /// document, so the id changes a step ahead of the binding. Measured
    /// by instrumenting both: with an id key the seed fires against the
    /// *outgoing* document while the incoming tab still reports
    /// `scintilla_doc == 0`, and is then skipped on the refresh that
    /// follows the real bind — so the buffer the user ends up looking at
    /// is never seeded at all. It happens to produce the right width
    /// when nothing repaints in between, which is what makes it a
    /// dangerous thing to adopt on inspection.
    ///
    /// A future document-pooling optimisation, or an `activate_tab` that
    /// released before allocating, would break the premise above. There
    /// is no drop-in key to switch to if that happens — a real identity
    /// would have to be threaded from the bind itself.
    static LAST_SEEDED_DOC: Cell<isize> = const { Cell::new(0) };
    /// Last dirty state pushed into the tab strip, so a notification
    /// storm only rebuilds it when the marker actually changes. See
    /// [`on_sci_notify`].
    static LAST_DIRTY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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

    // Notifications: caret moves, edits and save-point transitions. The
    // Cocoa analogue of GTK's `sci-notify` connection and Win32's
    // `WM_NOTIFY` handling. Registered before the window is shown so no
    // early edit is missed.
    //
    // SAFETY: `sci_ptr` is the live view; `on_sci_notify` matches
    // `SciNotifyFunc`'s signature and is `extern "C"`, so nothing can
    // unwind across the boundary from it (it only calls back into
    // `with_state`, whose failure mode is `None`, not a panic).
    unsafe {
        codepp_scintilla_sys::scintilla_cocoa_set_notify_callback(sci_ptr, on_sci_notify, 0);
    }

    // --- The window ------------------------------------------------
    let content_rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(DEFAULT_WIDTH, DEFAULT_HEIGHT),
    );
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;

    // An `NSWindow` *subclass*, and the subclass earns its keep: it
    // refuses keyboard focus to the Document Map's miniature, which is
    // bound to the same editable document as the editor and would
    // otherwise take Tab focus and accept typing with no visible caret.
    // See `crate::window` for the measurement.
    let window: Retained<NSWindow> =
        Retained::into_super(window::MainWindow::new(content_rect, style, mtm));

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

    // **Opt out of macOS's own window tabbing.** With the default
    // (`Automatic`), AppKit injects a "Show Tab Bar" / "Show All Tabs"
    // group into the View menu and, on request, adds a *second* tab bar
    // above the window's content showing one native tab per window. That
    // is a window-management feature, and Code++ is single-window by
    // design (§10) with its own document tab strip — so those menu items
    // offered a full-width bar that duplicated nothing useful and could
    // never contain more than one entry, while the real tab strip stayed
    // where it was. Reported as confusing, and it was: two things called
    // "tabs", one of them AppKit's and unrelated to the buffers.
    window.setTabbingMode(NSWindowTabbingMode::Disallowed);

    // **Pin the window to sRGB, which halves its compositing memory.**
    //
    // On a wide-gamut / EDR-capable display — every recent Mac —
    // CoreAnimation gives a window's backing surface a half-float RGBA
    // format (`RGhA`, 8 bytes per pixel) so it can represent colours
    // outside sRGB and beyond 1.0. Measured on this machine: three
    // 1926×1346 `RGhA` surfaces at 20.0 MB each. Pinning the colour
    // space drops them to `BGRA` at 4 bytes per pixel — 10.2 MB each,
    // and `phys_footprint` from ~77 MB to ~50 MB (49.5 / 49.5 / 50.2
    // across three release runs), a 35% saving on DESIGN.md §8's
    // most-missed budget. It scales with window area, so a larger
    // window saves proportionally more.
    //
    // **Nothing is lost, because Code++ has no wide-gamut content to
    // lose.** Every colour it draws is a plain 8-bit RGB triple: the
    // lexer themes in `codepp_editor::theme` are hex literals, Scintilla
    // takes `SCI_STYLESETFORE` as packed RGB, and the toolbar icons are
    // sRGB PNGs. Those values *are* sRGB, so rendering them through a
    // half-float EDR pipeline cannot make them more accurate — it only
    // makes the buffer wider.
    //
    // Two things that look like they would do the same and do not,
    // both measured: `setDepthLimit(NSWindowDepthTwentyfourBitRGB)`
    // changed nothing, and neither did `setOpaque(true)`. The format is
    // chosen from the colour space, not from the depth limit.
    window.setColorSpace(Some(&NSColorSpace::sRGBColorSpace()));
    // The tab strip's overflow arrows and scroll offset are only
    // recomputed by `TabStrip::sync`, which runs on tab-list events — so
    // a resize needs a hook of its own. See `Actions`'s
    // `windowDidResize:`.
    //
    // SAFETY: `Actions` implements `NSWindowDelegate`, and AppKit holds
    // the delegate weakly — the window state owns `actions` for the
    // process lifetime, so it cannot dangle.
    window.setDelegate(Some(ProtocolObject::from_ref(&*actions)));

    let (map_view, map_editor) = create_map_miniature()?;

    let (status, tab_strip, toolbar, fif_dock, docmap, workspace) = build_content(
        &window,
        content_rect,
        &sci_view,
        map_view,
        map_editor,
        &actions,
        mtm,
    );

    // --- Install the state ----------------------------------------
    let st = Rc::new(RefCell::new(CocoaUiState {
        window: window.clone(),
        sci_view: sci_view.clone(),
        sci_ptr,
        editor,
        status,
        tabs: tab_strip,
        toolbar,
        actions: actions.clone(),
        menu,
        fif_dock,
        fif_job: fif::FifJob::default(),
        docmap,
        workspace,
        find_replace: None,
        shell,
    }));
    install(&st);
    PERF.with(|p| *p.borrow_mut() = Some(Rc::new(perf)));

    // --- Startup work ---------------------------------------------
    apply_startup_styles();
    apply_editor_appearance();
    seed_horizontal_scroll(&editor);
    restore_session(initial_path);
    // MUST run *after* `restore_session` — that is what loads
    // session.xml into the shell, and this reads
    // `saved_window_geometry()` from it. Called earlier it would read an
    // empty session and silently do nothing. Same ordering constraint
    // GTK documents.
    restore_window_geometry(&window, mtm);
    // Same ordering constraint, same reason: the persisted View toggles
    // only become readable once session.xml is in the shell. See the
    // function.
    apply_saved_view_settings();
    // And the same again for the Document Map's width and open state.
    // After `apply_saved_view_settings` rather than before only because
    // opening the map relayouts the chrome, and doing that once at the
    // end is one fewer frame of the editor at the wrong width.
    docmap::apply_saved();
    // And the workspace panel's width, root and open state. After the
    // map for the same reason: each one that opens relayouts the chrome.
    workspace::apply_saved();

    // The auto-save timer is retained by the run loop; `actions` must
    // outlive it, which the binding below guarantees.
    let autosave = actions.start_autosave(mtm);

    // The opening edge of the §8 keystroke interval; see the function.
    // `None` when the state is somehow unreachable, which only costs a
    // measurement — never a failed startup.
    let key_probe = PERF
        .with(|p| p.borrow().clone())
        .and_then(|perf| install_key_probe(&sci_view, &window, &perf));

    // Ordering the window front and activating happens in the
    // application delegate's `applicationDidFinishLaunching:`, **not**
    // here. Doing it before `-[NSApplication run]` silently fails for a
    // process outside an `.app` bundle — the window never becomes key
    // and the app never becomes active, so the window server does not
    // route mouse clicks to it. See `crate::activate_main_window` and
    // the delegate for the measurements.

    with_state(|s| {
        let (_, ui) = s.split();
        ui.refresh_dynamic_status();
    });

    // **Cold start is *not* marked here.** It used to be, on the stated
    // grounds that "the window is on screen and the event loop is about
    // to take over" — a claim that was false by the time it was read.
    // Ordering the window front moved into the application delegate in
    // m3b (see `activate_main_window`), so at this point the window has
    // never been shown, the app has never been activated, and nothing
    // has painted. The mark also did not measure the same span as the
    // other two backends, which both close it on a real `SCN_PAINTED`.
    //
    // Measured, m4b: this point is reached at ~155–175 ms, the delegate
    // fires at ~180–230 ms, and the first paint lands at ~225–262 ms —
    // so the old figure under-reported the honest number by 60–95 ms.
    // The mark now lives in `on_sci_notify`'s `SCN_PAINTED` arm, which
    // is exactly where `ui_gtk` and `ui_win32` put theirs.

    // Keep the delegate, the action target and the timer alive for the
    // whole session. The reason is the *weak* references:
    // `NSApplication.delegate` and `NSMenuItem.target` are both unowned,
    // so nothing else holds `app_delegate` or `actions`. (`NSTimer` does
    // strongly retain its target, so `actions` is covered twice over —
    // the menu items are the binding constraint.) Binding to a named
    // local rather than `let _ =` is what pins them: `let _ =` drops
    // immediately.
    let _keepalive = (app_delegate, actions, autosave, key_probe);

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
///
/// **Not only modals.** Anything that spins a nested run loop needs
/// this, because GCD's main-queue source is serviced in those loops too.
/// The tab strip's drag-reorder tracking loop
/// (`tabs::TabButton::track`) is the non-modal case: it pumps
/// `NSEventTrackingRunLoopMode` by hand, and a worker completing
/// mid-drag would otherwise reach `refresh_tab_chrome` → `TabStrip::sync`,
/// which removes every control in the strip — including the button whose
/// `mouseDown:` is still on the stack running that loop — and could pop
/// a modal alert while the mouse button is still physically held.
pub(crate) struct DrainFreeze;

impl DrainFreeze {
    pub(crate) fn new() -> Self {
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

/// Run `f` at a boundary that native code calls into.
///
/// **Why every AppKit and Scintilla callback needs this.** A Rust panic
/// unwinding out of one of those entry points passes through frames that
/// are not Rust's — `objc_msgSend`, `-[NSApplication sendEvent:]`,
/// Scintilla's own C++ dispatch. `on_sci_notify` is declared plain
/// `extern "C"`, where a foreign unwind is undefined behaviour outright;
/// the `#[unsafe(method(...))]` overrides go through objc2's
/// `extern "C-unwind"` trampoline, which defines the *edge* but says
/// nothing about whether the AppKit frames above it survive a foreign
/// unwind. Neither is a risk worth carrying for the sake of a backtrace.
///
/// The rest of the workspace already treats this as non-negotiable —
/// `ui_win32`'s window and dialog procs, `ui_gtk`'s signal handlers and
/// `plugin-host`'s dispatcher all wrap their native-invoked boundaries
/// the same way (DESIGN.md §6.5 states the convention for the plugin
/// case). `ui_cocoa` had simply never adopted it.
///
/// A caught panic is logged and swallowed, and the caller gets
/// `fallback`. That is the right trade at a UI callback: the alternative
/// is aborting the process over, say, a paint that could not compute a
/// rectangle. It is not a licence to panic — nothing here is expected to,
/// and the log line is deliberately at `error` so one that does is not
/// mistaken for normal operation.
///
/// Note the release profile sets `panic = "abort"` (DESIGN.md §9.1), so
/// in a shipped build the process dies before any of this runs. This
/// earns its place in dev and test builds, which unwind by default —
/// i.e. exactly where a developer is most likely to trip a panic and
/// least likely to want undefined behaviour as the diagnostic.
pub(crate) fn at_callback_boundary<R>(
    entry: &'static str,
    fallback: R,
    f: impl FnOnce() -> R,
) -> R {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        tracing::error!(entry, "panic caught at a native callback boundary");
        fallback
    })
}

/// Scintilla's notification callback.
///
/// The Cocoa counterpart of GTK's `sci-notify` handler and Win32's
/// `WM_NOTIFY` arm. Registered once in [`run`]; see
/// `scintilla_cocoa_set_notify_callback` for why the entry point it goes
/// through is the deprecated one.
///
/// **This is on the keystroke path**, so what it does per notification
/// matters against DESIGN.md §8's 5 ms p99 budget:
///
///   * `SCN_UPDATEUI` and `SCN_MODIFIED` both refresh the status bar —
///     a handful of direct-calls plus label writes AppKit elides when
///     the string is unchanged. This is what makes Ln/Col/length track
///     typing instead of updating only when some unrelated model event
///     happened to repaint them. Both are handled because the two have
///     different timing; see the comment on the arm itself.
///   * `SCN_SAVEPOINTLEFT` / `SCN_SAVEPOINTREACHED` are the dirty
///     transitions. They are *edges*, not per-keystroke, so rebuilding
///     the strip there is cheap — but the strip is also rebuilt from
///     `SCN_UPDATEUI` when the cached marker disagrees with reality,
///     guarded by `LAST_DIRTY` so the common case is one `Cell` read.
///
/// Deliberately does **not** call `drain_shell`: that is the worker-wake
/// path, and running it per keystroke would poll channels thousands of
/// times a second for no reason.
///
/// # The re-entrancy contract this depends on
///
/// This fires synchronously from inside Scintilla, which may be inside
/// a `with_state` borrow *we* took — `Shell::drain` reaching
/// `replace_doc_text` → `SCI_SETTEXT` → `SCN_MODIFIED` is a live example.
/// `with_state` then declines (`try_borrow_mut` fails) and this handler
/// silently does nothing, which is why it cannot panic across the FFI
/// boundary — but it also means the refresh is *skipped* for that edit.
///
/// **So any code path that changes buffer text from inside a
/// `with_state` closure must refresh the chrome itself afterwards.**
/// Every current one does (`drain_shell` calls `refresh_dynamic_status`
/// inline and `refresh_tab_chrome` after the borrow; `run`'s startup
/// path calls `refresh_tab_chrome`). Nothing enforces it, so a new
/// caller that forgets would leave the status bar and dirty marker
/// stale with no other symptom.
///
/// # Safety
///
/// Called by Scintilla on the main thread. `lparam` is an
/// `SCNotification*` when `message` is [`COCOA_WM_NOTIFY`], live for the
/// duration of this synchronous call and owned by Scintilla.
unsafe extern "C" fn on_sci_notify(_windowid: isize, message: u32, _wparam: usize, lparam: usize) {
    // Plain `extern "C"`, so an escaping panic is undefined behaviour
    // rather than merely unwise. See [`at_callback_boundary`].
    at_callback_boundary("SCN_notify", (), || {
        // SAFETY: unchanged from the enclosing signature's contract —
        // `lparam` is the live `SCNotification` Scintilla passed.
        unsafe { on_sci_notify_inner(message, lparam) }
    });
}

/// The body of [`on_sci_notify`], so the panic guard wraps it whole.
///
/// # Safety
///
/// `lparam` must be a live `SCNotification*` for the duration of the
/// call, which is what Scintilla passes for `WM_NOTIFY`.
unsafe fn on_sci_notify_inner(message: u32, lparam: usize) {
    if message != codepp_scintilla_sys::COCOA_WM_NOTIFY || lparam == 0 {
        return;
    }
    // SAFETY: for `WM_NOTIFY` the Cocoa backend passes `&scn` — a live
    // `SCNotification` — and `Sci_NotifyHeader` is its `#[repr(C)]`
    // prefix, so this is a prefix read rather than a reinterpretation.
    // Valid only for this synchronous call, which is all it is used for.
    let code = unsafe { (*(lparam as *const codepp_scintilla_sys::Sci_NotifyHeader)).code };

    match code {
        // Both, not just `SCN_UPDATEUI`. `SCN_MODIFIED` is emitted
        // synchronously from the edit itself, whereas `SCN_UPDATEUI`
        // arrives on Scintilla's paint/idle pass (`Editor::Paint` →
        // `NotifyUpdateUI`). Measured, not assumed: the smoke test
        // drives an edit with no run loop running and observes
        // `SCN_SAVEPOINTLEFT` but zero `SCN_UPDATEUI`. Handling only the
        // latter would tie Ln/Col/length to paint timing rather than to
        // the edit — fine while the window is visible, wrong when it is
        // occluded or a batch of programmatic edits lands between
        // frames. The refresh is idempotent, so handling both costs a
        // repeat rather than risking a stale bar.
        codepp_scintilla_sys::SCN_UPDATEUI | codepp_scintilla_sys::SCN_MODIFIED => {
            // Promotes a pending key press to a real measurement. Only
            // for `SCN_MODIFIED` — `SCN_UPDATEUI` fires for a caret move
            // with no edit, and promoting there would time keys that
            // changed no text. See `codepp_core::perf`.
            if code == codepp_scintilla_sys::SCN_MODIFIED {
                if let Some(p) = PERF.with(|p| p.borrow().clone()) {
                    p.text_modified();
                }
            }
            with_state(|st| {
                let (_, ui) = st.split();
                ui.refresh_dynamic_status();
            });
            // The Document Map's orange box tracks the viewport, so it
            // moves on a scroll (`SCN_UPDATEUI`) and its extent changes
            // on an edit (`SCN_MODIFIED`). A no-op — one `Cell` read —
            // while the map is closed, which is the common case.
            //
            // On a keystroke that also flips the dirty bit this
            // recomputes twice, because `refresh_tab_chrome` below ends
            // in `sync_to_active_tab`. Left alone deliberately: that
            // path runs only on a dirty *edge*, and the alternative —
            // making one of the two conditional on the other — trades a
            // handful of direct-calls for a way to miss a refresh, which
            // is the failure this backend keeps having to fix.
            docmap::refresh();
            // Cheap guard: only touch the strip when the dirty marker
            // would actually change. Without this, every caret move
            // would rebuild every tab button.
            let Some(live) = active_dirty() else {
                return;
            };
            if LAST_DIRTY.with(std::cell::Cell::get) != live {
                LAST_DIRTY.with(|c| c.set(live));
                refresh_tab_chrome();
            }
        }
        // The container lexer's request for styling. Only a UDL buffer
        // can produce one — every other language is a Lexilla lexer,
        // which styles itself and never asks the host — so the handler
        // re-checks the active language and does nothing otherwise.
        //
        // `position` is read through `Sci_NotificationText`, the
        // `#[repr(C)]` prefix of the real notification, which is a prefix
        // read rather than a reinterpretation. Same trick GTK's
        // `style_needed_position` uses on its boxed payload.
        codepp_scintilla_sys::SCN_STYLENEEDED => {
            // SAFETY: as for the `code` read above — `lparam` is a live
            // `SCNotification*` for this synchronous call, and
            // `Sci_NotificationText` lays out `nmhdr` then `position` at
            // the same offsets `Scintilla.h` does.
            let position = unsafe {
                (*(lparam as *const codepp_scintilla_sys::Sci_NotificationText)).position
            };
            // A negative `position` would be a Scintilla contract
            // violation; treat it as nothing to style rather than
            // wrapping it into a huge range.
            if let Ok(target) = usize::try_from(position) {
                udl::on_style_needed(target);
            }
        }
        codepp_scintilla_sys::SCN_SAVEPOINTLEFT => {
            LAST_DIRTY.with(|c| c.set(true));
            refresh_tab_chrome();
        }
        codepp_scintilla_sys::SCN_SAVEPOINTREACHED => {
            LAST_DIRTY.with(|c| c.set(false));
            refresh_tab_chrome();
        }
        // Width tracking recomputes `scrollWidth` during layout, so the
        // floor has to be re-asserted after Scintilla has painted —
        // otherwise it lasts only until the next relayout. Cheap: one
        // read, and a write only when the width actually fell below the
        // viewport. See `clamp_scroll_width_to_viewport`.
        codepp_scintilla_sys::SCN_PAINTED => {
            // Cold start closes here, on a **real** first paint — the
            // same span `ui_gtk` and `ui_win32` measure, so the three
            // figures in §8 are finally the same quantity. `painted()`
            // closes any keystroke intervals waiting on this paint; see
            // `install_key_probe`, which opens them.
            //
            // No `hwndFrom`-style filter, unlike Win32 — and since m4c
            // that is a property of the *registration* rather than of
            // the backend. There are two Scintilla views now (the editor
            // and the Document Map's miniature), but only the editor has
            // a notification callback installed, so nothing else can
            // reach here. Registering one on the miniature would let a
            // map repaint close the editor's pending keystroke interval
            // and fabricate a cold-start figure; if a future milestone
            // needs miniature notifications, the filter has to land in
            // the same change. Same obligation for the plugin host's
            // `create_plugin_scintilla`.
            //
            // Before the layout repair below, which takes a `with_state`
            // borrow: the mark is a plain timestamp and must not be
            // hostage to a re-entrant decline.
            if let Some(p) = PERF.with(|p| p.borrow().clone()) {
                p.mark_first_draw();
                p.painted();
            }
            with_state(|st| {
                // Repair first, measure second. `clamp_scroll_width_to_viewport`
                // reads the clip's width, and on a paint that follows a
                // re-tile that width is the wrong, scroller-covering one —
                // measuring first would raise the horizontal floor above the
                // real viewport, and since that floor only ever rises it
                // would stay there.
                if let Some(mtm) = MainThreadMarker::new() {
                    enforce_scroller_layout(&st.sci_view, mtm);
                }
                clamp_scroll_width_to_viewport(&st.editor, &st.sci_view);
            });
        }
        _ => {}
    }
}

/// Assemble the window's content view and the three chrome strips.
///
/// Split out of [`run`] purely for length; the layout reasoning lives in
/// the comments below rather than at the call site.
/// Create the Document Map's miniature: the **second** permanent
/// Scintilla view, and the last one.
///
/// See `docmap`'s module docs for what it is, and the source scan in
/// [`source_invariants`] that pins the count at two. Like the main view
/// it is created once and never destroyed, removed or reassigned; it
/// shares each tab's document through `SCI_SETDOCPOINTER` rather than
/// owning any text of its own, which is what keeps the `Copy`,
/// lifetime-free `EditorHandle` it returns sound.
///
/// **Deliberately no notification callback is registered for it.** That
/// keeps `on_sci_notify` a single-view path: its `SCN_PAINTED` arm closes
/// the cold-start mark and any pending keystroke interval, and a
/// miniature repaint must not be able to close either.
fn create_map_miniature() -> Result<(Retained<NSView>, EditorHandle), CocoaUiError> {
    // SAFETY: same preconditions as the main view in `run` —
    // `NSApplication::sharedApplication` has run and this is the main
    // thread, both established there before this is called.
    let ptr = unsafe { scintilla_cocoa_new() };
    if ptr.is_null() {
        return Err(CocoaUiError::ScintillaCreate);
    }
    // Adopt the +1 the shim handed out before anything that can fail, so
    // an early return below releases it through `Drop` rather than
    // leaking. Same discipline as the main view.
    //
    // SAFETY: a non-null, +1-retained `ScintillaView`, an `NSView` subclass.
    let view: Retained<NSView> =
        unsafe { Retained::from_raw(ptr.cast::<NSView>()) }.ok_or(CocoaUiError::ScintillaCreate)?;
    // SAFETY: the non-null view just returned, still owned by `view`.
    let editor =
        unsafe { EditorHandle::from_cocoa_view(ptr) }.ok_or(CocoaUiError::DirectCallCapture)?;
    Ok((view, editor))
}

fn build_content(
    window: &NSWindow,
    content_rect: NSRect,
    sci_view: &NSView,
    map_view: Retained<NSView>,
    map_editor: EditorHandle,
    actions: &Actions,
    mtm: MainThreadMarker,
) -> (
    StatusBar,
    TabStrip,
    Toolbar,
    fif::FifDock,
    docmap::DocMapPanel,
    workspace::WorkspacePanel,
) {
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
    let toolbar = Toolbar::new(DEFAULT_WIDTH, actions, mtm);
    // Starts hidden and contributes no height until a search opens it,
    // so the initial layout is the same one m3 had.
    let fif_dock = fif::FifDock::new(DEFAULT_WIDTH, actions, mtm);

    let editor_height = DEFAULT_HEIGHT - STATUS_BAR_HEIGHT - TAB_STRIP_HEIGHT - TOOLBAR_HEIGHT;
    // Also hidden at first, and likewise contributes no width — so a
    // session that never opens the map lays out exactly as before.
    let docmap = docmap::DocMapPanel::new(map_view, map_editor, editor_height, actions, mtm);
    // Likewise hidden and contributing no width until a folder is opened.
    let workspace = workspace::WorkspacePanel::new(editor_height, actions, mtm);
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

    // The toolbar is the topmost strip, above the tabs — Notepad++'s
    // order, and `ui_gtk`'s. Same springs-and-struts treatment as the tab
    // strip: pinned to the top, width-sizable, flexible gap below.
    toolbar.container.setFrame(NSRect::new(
        NSPoint::new(0.0, STATUS_BAR_HEIGHT + editor_height + TAB_STRIP_HEIGHT),
        NSSize::new(DEFAULT_WIDTH, TOOLBAR_HEIGHT),
    ));
    toolbar.container.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
    );

    // Both side panels are parked off their own edge at their natural
    // width rather than squashed to zero — see the note in
    // `CocoaUi::relayout_chrome` for why zero is not the harmless choice
    // it looks like.
    docmap
        .container
        .setFrameOrigin(NSPoint::new(DEFAULT_WIDTH, STATUS_BAR_HEIGHT));
    workspace.container.setFrameOrigin(NSPoint::new(
        -workspace.container.frame().size.width,
        STATUS_BAR_HEIGHT,
    ));

    // **Every panel built above must be added here.** A view that is
    // never parented is not an error anyone notices: it keeps its frame,
    // reports `isHidden == false`, hands out its subviews, and answers
    // every question a probe thinks to ask — it simply never draws.
    // `workspace.container` shipped missing from this list and the panel
    // was a blank rectangle; the source scan
    // `every_panel_is_added_to_the_content_view` exists because of it.
    content.addSubview(sci_view);
    content.addSubview(&docmap.container);
    content.addSubview(&workspace.container);
    content.addSubview(&status.container);
    content.addSubview(&fif_dock.container);
    content.addSubview(&tab_strip.container);
    content.addSubview(&toolbar.container);
    window.setContentView(Some(&content));

    (status, tab_strip, toolbar, fif_dock, docmap, workspace)
}

/// Observe key presses so `--perf` can measure keystroke latency.
///
/// The opening edge of the DESIGN.md §8 interval. The other two edges
/// already exist — `SCN_MODIFIED` promotes a press to a real
/// measurement and `SCN_PAINTED` closes it — so this is what makes the
/// macOS latency row measurable at all; before it, `painted()` closed
/// intervals nothing ever opened.
///
/// **A local event monitor, not a subclass.** Win32 matches `WM_CHAR`
/// in its own message pump and GTK connects `key-press-event` to the
/// Scintilla widget. Neither shape exists here: the pump is
/// `-[NSApplication run]`, and the view that receives keys is
/// `SCIContentView` inside the vendored Scintilla tree, which
/// DESIGN.md §4.1 keeps unforked. A local monitor sees every key event
/// this process is about to dispatch, and returning the event unchanged
/// leaves delivery exactly as it was — the same "observe, never
/// swallow" rule `ui_gtk`'s handler documents.
///
/// Two filters, matching the other backends:
///
/// * **The editor must be the first responder.** Without it, typing in
///   the Find panel's query field would open intervals that no editor
///   paint ever closes; they would sit in `pending` until some unrelated
///   repaint closed them and report the time spent in the dialog as
///   keystroke latency. `ui_win32` documents the identical hazard for
///   its own dialogs.
/// * **⌘ and ⌃ chords are excluded**, being the macOS equivalents of the
///   Ctrl chord §8 excludes: they are commands, not typed characters,
///   and a paste's redraw is a different quantity large enough to
///   dominate the tail. ⌥ is *not* excluded — it composes real
///   characters (⌥e, ⌥u), exactly as `ui_gtk` reasons about `AltGr`.
///
/// Returns the monitor, which the caller must keep alive; dropping it
/// unregisters the observation.
fn install_key_probe(
    sci_view: &NSView,
    window: &NSWindow,
    perf: &Rc<Perf>,
) -> Option<Retained<objc2::runtime::AnyObject>> {
    if !perf.enabled() {
        return None;
    }
    let perf = Rc::clone(perf);
    let sci_view = sci_view.retain();
    let window = window.retain();
    let handler = block2::RcBlock::new(move |event: std::ptr::NonNull<NSEvent>| {
        // SAFETY: AppKit hands the monitor a live, autoreleased event
        // for the duration of the call.
        let event: &NSEvent = unsafe { event.as_ref() };
        at_callback_boundary("perf:keyDown", (), || {
            if event_is_typed_into_editor(event, &window, &sci_view) {
                perf.key_pressed();
            }
        });
        // Unmodified: this observes, it must never swallow.
        std::ptr::from_ref::<NSEvent>(event).cast_mut()
    });
    // SAFETY: the block outlives the call — `RcBlock` is refcounted and
    // AppKit retains the handler for the monitor's lifetime.
    unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &handler) }
}

/// Whether a key event is a character typed into the editor, rather than
/// a command chord or a keystroke aimed at some other view.
fn event_is_typed_into_editor(event: &NSEvent, window: &NSWindow, sci_view: &NSView) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    if event.window(mtm).as_deref() != Some(window) {
        return false;
    }
    let Some(responder) = window.firstResponder() else {
        return false;
    };
    let Ok(view) = responder.downcast::<NSView>() else {
        return false;
    };
    if !view.isDescendantOf(sci_view) {
        return false;
    }
    modifiers_type_a_character(event.modifierFlags())
}

/// Whether a key event's modifiers describe a *typed character* rather
/// than a command chord.
///
/// Split out so the decision is unit-testable without a live `NSEvent`,
/// a window and a first responder — the interesting part is which flags
/// are in the set, and a future edit flipping it would otherwise change
/// what §8's latency figures mean with nothing to catch it.
///
/// ⌘ and ⌃ are the macOS equivalents of the Ctrl chord §8 excludes. **⌥
/// is deliberately not excluded**, and the reason differs from GTK's:
/// there, `Ctrl` is excluded only when `Alt` is absent, because Ctrl+Alt
/// is `AltGr` and types real characters. macOS has no Control-based
/// composition, so Control can be excluded outright — but Option alone
/// composes (⌥e, ⌥u), so it must not be.
fn modifiers_type_a_character(flags: NSEventModifierFlags) -> bool {
    !flags.intersects(NSEventModifierFlags::Command | NSEventModifierFlags::Control)
}

/// Order the main window front, focus the editor, and activate the app.
///
/// Called from the application delegate's
/// `applicationDidFinishLaunching:` rather than from [`run`]. Doing it
/// before `-[NSApplication run]` has no effect for a non-bundled
/// process; the delegate carries the measurements that established
/// that, and the symptom it produces (mouse dead, keyboard apparently
/// fine) is misleading enough to be worth reading before moving this.
pub(crate) fn activate_main_window() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    with_state(|st| {
        // Focus the editor so the first keystroke lands in the buffer
        // rather than on a tab-strip button.
        st.window.makeFirstResponder(Some(&st.sci_view));
        st.window.makeKeyAndOrderFront(None);
    });
    NSApplication::sharedApplication(mtm).activate();
}

/// Emit the `--perf` distribution. Called from the application
/// delegate's `applicationWillTerminate:`, which is the only shutdown
/// hook `terminate:` actually reaches.
pub(crate) fn report_perf() {
    if let Some(p) = PERF.with(|p| p.borrow().clone()) {
        p.report();
    }
}

/// Seed the horizontal scroll width, and enable tracking.
///
/// **Both calls, always — the second is what makes the first safe.**
/// Scintilla seeds `scrollWidth` at 2000 px and never shrinks it, so
/// with word wrap off the user can scroll far past the longest line.
/// Width *tracking* makes Scintilla recompute `scrollWidth` from the
/// longest visible line, and seeding it at 1 px stops the 2000 default
/// carrying into the first paint.
///
/// Shipping the reset without tracking is a bug with a very confusing
/// symptom on this backend, and it shipped once: on Cocoa `scrollWidth`
/// directly sizes the `NSScrollView`'s document view, so a
/// `SCI_SETSCROLLWIDTH(1)` with nothing to recompute it collapses
/// `SCIContentView` to **one point wide**. Hit-testing then lands on the
/// enclosing `NSClipView` for every click past x=1, Scintilla never sees
/// a `mouseDown:`, and the editor looks like it has stopped responding
/// to the mouse — while the keyboard, which goes to the first responder
/// rather than through hit-testing, keeps working. GTK does not show
/// this because its backend does not size a document view from
/// `scrollWidth`.
pub(crate) fn seed_horizontal_scroll(editor: &EditorHandle) {
    editor.send(codepp_scintilla_sys::SCI_SETSCROLLWIDTH, 1, 0);
    editor.send(codepp_scintilla_sys::SCI_SETSCROLLWIDTHTRACKING, 1, 0);
}

/// Widen `scrollWidth` so the document view always covers the visible
/// text area.
///
/// **Why this is needed at all.** Scintilla's Cocoa backend sizes the
/// `NSScrollView`'s document view from `scrollWidth`
/// (`cocoa/ScintillaCocoa.mm::SetScrollingSize`) and explicitly clamps
/// the *height* to the clip rect — "Ensure all of clipRect covered by
/// Scintilla drawing" — but does **not** do the same for the width:
/// `docWidth = Wrapping() ? clipRect.width : scrollWidth`. With width
/// tracking on, `scrollWidth` is the longest *visible* line, so the
/// document view ends exactly where the longest line ends and every
/// click to the right of it lands on the enclosing `NSClipView` instead.
///
/// The user-visible effect is that only the text itself is clickable:
/// double-clicking in the empty space right of a line to select it — an
/// ordinary editing gesture that works on Win32 and GTK — does nothing.
/// Neither of those backends shows it, because neither sizes a view from
/// `scrollWidth`; the whole editor widget is the click target there.
///
/// Fixed host-side rather than by patching the vendored tree, which
/// DESIGN.md §4.1 keeps unforked. Tracking stays on, so a genuinely long
/// line still scrolls horizontally and the width still shrinks back when
/// that line is deleted; this only raises the floor.
pub(crate) fn clamp_scroll_width_to_viewport(editor: &EditorHandle, sci_view: &NSView) {
    let Some(visible) = text_area_width(sci_view) else {
        return;
    };
    // Round down: a floor one pixel under the clip width leaves no
    // dead strip, whereas overshooting would create a scrollable range
    // that does not exist.
    let target = visible.floor() as isize;
    if target <= 0 {
        return;
    }
    let previous = LAST_VIEWPORT_WIDTH.with(|c| c.replace(target));
    let current = editor.send(codepp_scintilla_sys::SCI_GETSCROLLWIDTH, 0, 0);

    // **When the viewport shrinks, let the floor come down with it — but
    // only when the floor is all that is holding the width up.**
    //
    // This function otherwise only ever raises `scrollWidth`, which is
    // right while the window keeps its size: the floor is what stops the
    // blank area right of short lines being unclickable. The raised
    // value outlives the viewport that justified it, though, so
    // maximising and un-maximising a three-line file left the document
    // as wide as the *old* clip and a horizontal scrollbar appeared for
    // content that was not there.
    //
    // The test for "nothing but the floor" is that the width is exactly
    // the value this function last installed. A content-derived width
    // never matches that except by coincidence, and a coincidence is
    // harmless: tracking re-raises it on the next paint while the long
    // line is still on screen.
    //
    // **Re-seeding instead would be wrong, and was.** An earlier version
    // reset the width to 1 so tracking could recompute — but
    // `SCI_SETSCROLLWIDTH` also zeroes `lineWidthMaxSeen`, and tracking
    // only re-raises during a subsequent `Paint`, so reading the width
    // back in the same pass and clamping it to the viewport pinned it
    // there. Measured: with a 4 000-character line on screen, resizing
    // 1500 → 900 collapsed the document from 26 525 pt to 839 pt and it
    // did not recover, making the rest of that line unreachable.
    if previous > target && current == previous {
        editor.send(codepp_scintilla_sys::SCI_SETSCROLLWIDTH, target as usize, 0);
        return;
    }
    if current < target {
        editor.send(codepp_scintilla_sys::SCI_SETSCROLLWIDTH, target as usize, 0);
    }
}

/// Width of the scrolling text area, read from the `NSClipView` that
/// actually bounds it.
///
/// Taken from the live view rather than computed as "view width minus
/// margins minus scroller": the clip view already accounts for the
/// margin view, the vertical scroller and any inset, so reading it is
/// both exact and immune to those changing.
fn text_area_width(sci_view: &NSView) -> Option<f64> {
    editor_scroll_view(sci_view).map(|scroll| scroll.contentView().bounds().size.width)
}

/// Selection background, in Scintilla's `0xAABBGGRR` element-colour
/// layout: opaque light grey (`#C8C8C8`).
///
/// **Set explicitly because the Cocoa backend picks its own, and its
/// choice is wrong for a code editor.** `ScintillaCocoa::UpdateBaseElements`
/// derives `Element::SelectionBack` from the system's selected-text
/// colour — the accent blue — which is right for prose in a text field
/// and poor over syntax-highlighted code, where it swamps the foreground
/// colours the lexer just assigned. Win32 and GTK install no such base,
/// so they keep Scintilla's neutral grey; this brings macOS to the same
/// place rather than leaving one backend visibly different.
///
/// An explicit `SCI_SETELEMENTCOLOUR` is what it takes: the Cocoa value
/// is installed as the element *base*, and a base is only overridden by
/// an explicit element colour, not by the older `SCI_SETSELBACK`.
const SELECTION_BACK: usize = 0xFF_C8_C8_C8;

/// The inactive-view selection, a shade lighter so an unfocused editor
/// reads as unfocused — the same relationship the platform defaults have.
const SELECTION_BACK_INACTIVE: usize = 0xFF_DC_DC_DC;

/// Give the editor the selection colours, and stop its subviews painting
/// outside its band.
///
/// Also forces permanently visible scrollers and repairs the layout
/// that breaks — see [`enforce_scroller_layout`].
///
/// **The clip is not cosmetic tidiness.** `SCIScrollView` is a *flipped*
/// view, and on macOS 26 it carries a scroll-edge-effect
/// `NSVisualEffectView` sized 690 pt inside a 548 pt scroll view — so in
/// flipped coordinates that child extends ~140 pt *below* the editor's
/// own frame. AppKit does not clip subviews to their parent's bounds by
/// default, so that overflow paints over the status bar. Measured, not
/// inferred: the view dump shows the 690/548 mismatch and the horizontal
/// scroller at y=531 of 548, which is only the bottom edge if the view is
/// flipped.
///
/// Clipping the Scintilla view states the invariant the layout already
/// assumes — the editor draws inside the band it was given — rather than
/// chasing whichever AppKit-internal subview currently overflows.
fn apply_editor_appearance() {
    let mtm = MainThreadMarker::new();
    with_state(|st| {
        for element in [
            codepp_scintilla_sys::SC_ELEMENT_SELECTION_BACK,
            codepp_scintilla_sys::SC_ELEMENT_SELECTION_ADDITIONAL_BACK,
        ] {
            st.editor.send(
                codepp_scintilla_sys::SCI_SETELEMENTCOLOUR,
                element as usize,
                SELECTION_BACK as isize,
            );
        }
        st.editor.send(
            codepp_scintilla_sys::SCI_SETELEMENTCOLOUR,
            codepp_scintilla_sys::SC_ELEMENT_SELECTION_INACTIVE_BACK as usize,
            SELECTION_BACK_INACTIVE as isize,
        );
        st.sci_view.setClipsToBounds(true);
        if let Some(scroll) = editor_scroll_view(&st.sci_view) {
            // Permanent bars, matching Win32 and GTK. The layout this
            // then breaks is repaired below and after every paint.
            scroll.setScrollerStyle(NSScrollerStyle::Legacy);
            scroll.setAutohidesScrollers(false);
        }
        // After the style, never before: the repair is a no-op while the
        // scrollers are still overlay.
        if let Some(mtm) = mtm {
            enforce_scroller_layout(&st.sci_view, mtm);
        }
    });
}

/// Give the editor permanently visible scrollers, and repair the
/// layout Scintilla's own `tile` gets wrong when they are.
///
/// **Why this is a host-side repair rather than a style setting.**
/// Setting `NSScrollerStyle::Legacy` alone is not enough, and shipping
/// it alone was a bug: `SCIScrollView::tile` shifts the content view
/// right by the line-number margin's width and shrinks it by the same,
/// but nothing there accounts for scrollers that take space rather than
/// float. `ScintillaCocoa::SetScrollingSize` re-assigns
/// `hasVerticalScroller`/`hasHorizontalScroller` on every call, so the
/// first long line to widen the document re-tiled the view and left the
/// content view covering the scrollers — both bars gone. Measured:
/// the clip went from 1019×531 to 1036×548 inside a 1080×548 scroll
/// view, while the scroller frames stayed where they were.
///
/// The vendored `//[scrollView setScrollerStyle:NSScrollerStyleLegacy];`
/// at `ScintillaView.mm:1474` says upstream hit this too and backed out.
/// Code++ cannot take the other exit — leaving the scrollers on their
/// overlay default means they are drawn only during a wheel or trackpad
/// gesture, and Scintilla scrolls its own content, so keyboard
/// navigation, Go To Line and plugin scrolls produce nothing at all.
/// `flashScrollers` was tried for that and is not enough either: the
/// bars appear for a moment and the user reported seeing nothing.
///
/// So the fix is to state the geometry `tile` should have produced and
/// re-assert it after every paint. That is the same shape as
/// [`clamp_scroll_width_to_viewport`] — a vendored gap corrected from
/// the host because §4.1 keeps the tree unforked — and it is idempotent,
/// so re-running it costs one rect comparison when nothing moved.
///
/// Only touches the layout when the style really is Legacy: if the
/// system is set to overlay scrollers *and* this function has not forced
/// otherwise, the vendored arithmetic is correct as written and must be
/// left alone.
fn enforce_scroller_layout(sci_view: &NSView, mtm: MainThreadMarker) {
    let Some(scroll) = editor_scroll_view(sci_view) else {
        return;
    };
    if scroll.scrollerStyle() != NSScrollerStyle::Legacy {
        return;
    }
    let clip = scroll.contentView();
    let bounds = scroll.bounds();
    // The clip's x is the line-number margin's thickness, which
    // `SCIScrollView::tile` writes and which is the one part of its
    // arithmetic that is right. Read it back rather than recomputing:
    // the margin width is Scintilla's to decide.
    //
    // It reads 0 before `tile` has ever run, which is indistinguishable
    // from "no gutter". Harmless: the call from `apply_editor_appearance`
    // happens before the window is on screen, and the first real
    // `SCN_PAINTED` — which necessarily follows a tile — corrects it
    // before anything is visible.
    let margin = clip.frame().origin.x;
    // The scrollers' own widths rather than
    // `scrollerWidthForControlSize:scrollerStyle:` with an assumed
    // control size: an accessibility setting can change that, and being
    // a few points out would be *stable* rather than self-correcting,
    // because the idempotency check below would settle on it happily.
    let (width, height) = scroller_clip_size(
        bounds.size.width,
        bounds.size.height,
        margin,
        if scroll.hasVerticalScroller() {
            scroller_thickness(scroll.verticalScroller().as_deref(), mtm, false)
        } else {
            0.0
        },
        if scroll.hasHorizontalScroller() {
            scroller_thickness(scroll.horizontalScroller().as_deref(), mtm, true)
        } else {
            0.0
        },
    );
    let have = clip.frame();
    // Size only: the origin is `tile`'s to set and is the part it gets
    // right, so correcting it too would fight Scintilla over the gutter.
    // Compared rather than assigned unconditionally because this runs on
    // every paint and setting a frame invalidates.
    if (have.size.width - width).abs() > 0.5 || (have.size.height - height).abs() > 0.5 {
        clip.setFrame(NSRect::new(
            NSPoint::new(margin, 0.0),
            NSSize::new(width, height),
        ));
    }
    // The line-number gutter is an `NSRulerView` (`SCIMarginView`), laid
    // out by `NSScrollView` rather than by Scintilla, and it is given the
    // scroll view's **full** height — so with a horizontal scroller on
    // screen it runs 17 pt further down than the text does, and paints a
    // line number level with the scrollbar for a line whose text is not
    // there. Measured before the fix: ruler 690 pt tall inside a clip of
    // 673, with the scroller occupying 673..690.
    //
    // Trimming it to the clip's height leaves that corner unpainted, so
    // it shows the scroll view's own background — which is what the strip
    // should look like beside a scrollbar. Same idempotent
    // compare-then-set as the clip above, for the same reason: this runs
    // on every paint.
    if let Some(ruler) = scroll.verticalRulerView() {
        let have = ruler.frame();
        if (have.size.height - height).abs() > 0.5 {
            ruler.setFrame(NSRect::new(
                have.origin,
                NSSize::new(have.size.width, height),
            ));
        }
        // **Clipped, because Scintilla paints the gutter a whole line-row
        // at a time and AppKit does not clip a view to its own bounds by
        // default.** Trimming the ruler leaves a partial row at the
        // bottom, which `PaintMargin` still fills in full — reported as
        // the gutter "spilling onto the area below", and measured from a
        // screenshot as a 16 × 3 pt patch of margin grey stranded past
        // the ruler's real bottom edge. Same one-liner, for the same
        // reason, as the `setClipsToBounds` on the Scintilla view itself.
        //
        // Read before writing: this runs on every paint, and a setter
        // that marks the view dirty each time would be a repaint per
        // frame.
        if !ruler.clipsToBounds() {
            ruler.setClipsToBounds(true);
        }
    }
}

/// How much of the content a scroller takes, measured from the scroller
/// itself where there is one.
fn scroller_thickness(
    scroller: Option<&NSScroller>,
    mtm: MainThreadMarker,
    horizontal: bool,
) -> f64 {
    scroller.map_or_else(
        || {
            NSScroller::scrollerWidthForControlSize_scrollerStyle(
                NSControlSize::Regular,
                NSScrollerStyle::Legacy,
                mtm,
            )
        },
        |s| {
            let size = s.frame().size;
            if horizontal {
                size.height
            } else {
                size.width
            }
        },
    )
}

/// The size `SCIScrollView::tile` should have given the clip view.
///
/// Pure, and separated for that reason: rect arithmetic with two axes
/// and three subtracted terms is the shape that fails by an off-by-one
/// or a transposed axis, and those are exactly the failures a hands-on
/// demo is worst at catching. Same reasoning as the tab strip's
/// drop-index and shuffle helpers.
///
/// Floors at zero, so a window dragged smaller than its own chrome
/// cannot ask for a negative size.
fn scroller_clip_size(
    view_width: f64,
    view_height: f64,
    margin: f64,
    vertical_bar: f64,
    horizontal_bar: f64,
) -> (f64, f64) {
    (
        (view_width - margin - vertical_bar).max(0.0),
        (view_height - horizontal_bar).max(0.0),
    )
}

/// The `NSScrollView` inside a `ScintillaView`.
///
/// Found by walking the subviews rather than by index: it is vendored
/// code's view hierarchy, and a position is a weaker assumption than a
/// type.
fn editor_scroll_view(sci_view: &NSView) -> Option<Retained<objc2_app_kit::NSScrollView>> {
    sci_view
        .subviews()
        .iter()
        .find_map(|sub| sub.downcast::<objc2_app_kit::NSScrollView>().ok())
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
    // After `Shell::drain`, which is what consumes the in-open-buffer
    // replacements before the dock's own drain can see them.
    fif::drain_into_dock();
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

/// Whether the active buffer should paint as having unsaved changes.
///
/// **Two sources, not one.** `SCI_GETMODIFY` is the live editor bit and
/// covers ordinary typing, but a buffer restored from a crash-recovery
/// backup sits at its Scintilla save point while still having no copy on
/// disk. `Shell` tracks those in `unsaved_restore_ids`, and without the
/// OR below, editing one character in a restored buffer and undoing it
/// returns the document to that seeded save point and the marker flips
/// to clean — for a buffer whose contents exist nowhere but memory.
///
/// DESIGN.md §7.4 records exactly this as an open Win32 bug and notes
/// that `ui_gtk`'s `refresh_active_dirty` is the correct one because it
/// does this OR. The Cocoa port inherited the Win32 shape by omission;
/// this is where it stops. No data was ever at risk on any backend —
/// `Shell::tab_needs_backup` ORs the same id set, so the recovery backup
/// is retained regardless — but the indicator was wrong.
///
/// Returns `None` when the state is unreachable (a re-entrant call), so
/// callers can leave the cached value alone rather than treating "could
/// not look" as "clean". That distinction is the one `ui_gtk`'s
/// `DirtyPoll::Unavailable` exists to preserve.
fn active_dirty() -> Option<bool> {
    with_state(|st| {
        let modified = st.editor.send(codepp_scintilla_sys::SCI_GETMODIFY, 0, 0) != 0;
        // Read the id out before any mutable borrow: `is_unsaved_restore`
        // takes `&self`.
        let restored = st
            .shell
            .active_tab
            .and_then(|idx| st.shell.tabs.get(idx))
            .is_some_and(|t| st.shell.is_unsaved_restore(t.id));
        modified || restored
    })
}

/// Write the currently-bound document's dirty state onto the tab that
/// owns it.
///
/// **Ordering is the whole contract: this must run while `active_tab`
/// still names the tab whose document is bound.** It reads the modify
/// bit of whatever document the single Scintilla view currently holds
/// and attributes it to whatever tab is currently active, so running it
/// after a switch records the *outgoing* buffer's dirtiness against the
/// *incoming* tab. Hence the calls in `select_tab_by_id` and
/// `close_tab_by_id` sit above the `active_tab` write, not below it.
///
/// Only the active tab is touched, because it is the only one whose
/// document is bound to the view. Inactive tabs keep the value captured
/// on their way out — which is exactly what this exists to make true.
/// `ui_gtk` has the same function for the same reason; the Cocoa port
/// was relying on `refresh_tab_chrome` happening to run at the right
/// moment, which it does not on a tab switch.
fn capture_active_dirty() {
    let Some(dirty) = active_dirty() else {
        // Unreachable state (a re-entrant call). Leave the cached marker
        // alone rather than writing "clean" from a read that never
        // happened.
        return;
    };
    with_state(|st| {
        if let Some(idx) = st.shell.active_tab {
            if let Some(tab) = st.shell.tabs.get_mut(idx) {
                tab.dirty = dirty;
            }
        }
        // Keep the notification handler's cached edge in step, so a
        // model-driven refresh (a tab switch, a save) does not leave
        // `LAST_DIRTY` disagreeing with what was just painted and cause
        // a redundant rebuild on the next caret move.
        LAST_DIRTY.with(|c| c.set(dirty));
    });
}

/// Resync the tab strip and the window title from the shell.
///
/// Called after anything that can change the tab list or which tab is
/// active — a drain, a tab switch, a close, an open. Cheap enough to
/// call unconditionally (see `TabStrip::sync` on why it rebuilds
/// wholesale) and never on the keystroke path.
/// Re-lay the window's chrome bands from outside a `with_state` borrow.
///
/// `CocoaUi::relayout_chrome` is a method on the split value precisely
/// because every `UiPlatform` method already holds the borrow; this is
/// the entry point for the callers that do *not* — the window-resize
/// delegate and the dock's own open/close. Declined re-entrantly like
/// any other `with_state` call, which is correct: an outer caller is
/// mid-update and will lay out when it finishes.
pub(crate) fn relayout_chrome_bands() {
    with_state(|st| {
        let (_, ui) = st.split();
        ui.relayout_chrome();
    });
}

/// Re-seed the horizontal scroll range when, and only when, the view has
/// come to rest on a different document.
///
/// **`scrollWidth` belongs to the view, not to the document.** It is a
/// tracking-mode high-water mark shared by every buffer this one
/// Scintilla view holds, and it never shrinks on its own — so a document
/// swapped in behind a file with long lines inherits that file's width
/// and shows a horizontal scrollbar for content it does not have.
/// Re-seeding drops it to 1 and lets tracking rediscover it, with the
/// viewport floor restored in the same breath (the reset alone collapses
/// the document view to one point wide, which on this backend costs the
/// mouse — see `clamp_scroll_width_to_viewport`).
///
/// **Keyed on the bound document, and checked here rather than at the
/// swap.** Seeding inside `UiPlatform::activate_tab` — the swap itself —
/// looks like the tighter place for it and is wrong: `Shell::save_all`
/// binds each titled tab in turn purely so it can read the buffer text,
/// then rebinds the one the user was actually on. Those are bookkeeping
/// swaps, not navigation, and seeding on them resets the horizontal
/// scroll *position* of a buffer the user never left. Measured: with a
/// buffer scrolled to x=1500, File → Save All returned it to 0.
///
/// Checking once the binding has settled avoids that without having to
/// tell the two kinds of swap apart, because a transient swap restores
/// the original document before this ever runs — the pointer it sees is
/// the one it saw last, and it does nothing.
fn seed_horizontal_scroll_if_document_changed() {
    with_state(|st| {
        let doc = st
            .editor
            .send(codepp_scintilla_sys::SCI_GETDOCPOINTER, 0, 0);
        if LAST_SEEDED_DOC.with(|c| c.replace(doc)) == doc {
            return;
        }
        seed_horizontal_scroll(&st.editor);
        clamp_scroll_width_to_viewport(&st.editor, &st.sci_view);
    });
}

pub(crate) fn refresh_tab_chrome() {
    // Before anything paints: this runs after every drain, open, tab
    // switch, close and File → New, which is exactly the set of moments
    // the bound document can have changed.
    seed_horizontal_scroll_if_document_changed();
    // Pull the live modified bit into the active tab before painting, so
    // the strip's dirty marker reflects reality. `Tab.dirty` is only
    // ever written by the shell's crash-recovery restore paths, so
    // without this it would not reflect editing at all — the same gap
    // `confirm_discard_active` closes the same way. Since m3b wired
    // Scintilla's notifications, `on_sci_notify` also drives this on the
    // dirty *edges*; this call is what keeps the two in agreement when a
    // model event (a tab switch, a save) repaints outside that path.
    capture_active_dirty();
    with_state(|st| {
        let mtm = MainThreadMarker::new();
        if let Some(mtm) = mtm {
            let active = st.shell.active_tab;
            // Before the rebuild, so the strip is laid out at the offset
            // that shows the active tab. Without it the arrows only solve
            // half the problem: selecting a scrolled-off tab from the
            // Window menu, or closing one, would leave the selection
            // somewhere the user cannot see.
            //
            // **Only when the active buffer actually changed.** This runs
            // on every chrome refresh — a save, a keystroke's dirty edge,
            // a reorder — and scrolling unconditionally made the arrows
            // inert: each press moved the offset and the refresh that
            // followed dragged it straight back to the active tab.
            // Keyed on `Tab.id` rather than the index so a reorder, which
            // changes the index without changing the buffer, does not
            // count as a switch.
            let active_id = active.and_then(|i| st.shell.tabs.get(i)).map(|t| t.id);
            if active_id.is_some() && LAST_SCROLLED_TO.with(Cell::get) != active_id {
                LAST_SCROLLED_TO.with(|c| c.set(active_id));
                if let Some(index) = active {
                    st.tabs.scroll_into_view(index, st.shell.tabs.len());
                }
            }
            // Clone the receiver out first: `sync` borrows the tab list
            // immutably while it reads, and `actions` lives on the same
            // struct.
            let actions = st.actions.clone();
            st.tabs.sync(&st.shell.tabs, active, &actions, mtm);
        }
    });
    // After the strip, and outside the borrow above: rebinding the
    // miniature takes its own `with_state`. This runs after every drain,
    // open, tab switch, close and File → New — the same set of moments
    // the bound document can have changed, which is exactly when the map
    // needs to be re-pointed.
    docmap::sync_to_active_tab();
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
    // The horizontal scroll range is re-seeded by `refresh_tab_chrome`
    // below, which does it for every path rather than only this one —
    // see `seed_horizontal_scroll_if_document_changed`.
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
        // Already active. Still re-sync the strip rather than returning
        // outright, so the painted selection is always re-derived from
        // the model — the same "never infer selection from the control's
        // own state" rule `ui_gtk`'s strip documents. Cheap, and it keeps
        // one code path responsible for what the strip shows.
        refresh_tab_chrome();
        return;
    }
    // Before the switch, never after — see [`capture_active_dirty`].
    capture_active_dirty();
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
        // Before the switch, never after — see [`capture_active_dirty`].
        capture_active_dirty();
        with_state(|st| st.shell.active_tab = Some(idx));
        rebind_active_view();
    }
    action_close_tab();
}

/// Step the tab strip one tab left or right — the overflow arrows.
///
/// The strip is rebuilt from the model afterwards rather than having its
/// buttons repositioned, which is the same total-resync approach every
/// other change to it takes.
pub(crate) fn scroll_tabs(forward: bool) {
    with_state(|st| {
        let count = st.shell.tabs.len();
        st.tabs.scroll_by_one(forward, count);
    });
    refresh_tab_chrome();
}

/// Whether the tab with `id` is pinned. `false` if it has since closed.
///
/// Read by the tab strip before it starts drag tracking: a pinned tab is
/// fixed in place, so the drag must not begin at all. `Shell::move_tab`
/// would reject the move regardless — this is what stops the user
/// dragging a tab that can never land, which is what `ui_gtk` achieves
/// by clearing `set_tab_reorderable` on pinned pages.
pub(crate) fn tab_is_pinned(id: i32) -> bool {
    with_state(|st| st.shell.tabs.iter().any(|t| t.id == id && t.pinned)).unwrap_or(false)
}

/// Toggle the pin state of the tab with `id` — the tab-strip pin glyph.
///
/// `Shell::set_pinned` flips the flag and relocates the tab into or out
/// of the pinned cluster (adjusting `active_tab` so the active buffer
/// follows its tab), then the strip re-renders in the new order. A no-op
/// if the tab has since closed.
pub(crate) fn toggle_pin_by_id(id: i32) {
    // The id→index lookup and the `set_pinned(idx, …)` mutation MUST stay
    // in one `with_state` closure: splitting them across two borrows
    // would let a tab move between them and pin the wrong buffer — the
    // same index staleness the id-keying guards against. `set_pinned`
    // range-checks `idx` itself.
    with_state(|st| {
        if let Some(idx) = st.shell.tabs.iter().position(|t| t.id == id) {
            let want = !st.shell.tabs[idx].pinned;
            st.shell.set_pinned(idx, want);
        }
    });
    refresh_tab_chrome();
}

/// Move the tab with `id` to position `target` — the drop half of a
/// tab-strip drag.
///
/// `Shell::move_tab` enforces the pinned-prefix invariant and declines a
/// move that would break it. Either outcome ends in the same
/// `refresh_tab_chrome`, which rebuilds the strip from the model: on
/// success it shows the new order, and on rejection it puts the dragged
/// button back where the model says it belongs. That is why nothing here
/// needs to undo the drag's visual displacement.
pub(crate) fn reorder_tab_by_id(id: i32, target: usize) {
    // Same single-borrow discipline as `toggle_pin_by_id`: resolve the id
    // to an index and move in one closure, so no tab can shift between
    // the two steps.
    with_state(|st| {
        let Some(from) = st.shell.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        // The strip derives `target` from a pixel position, so it can
        // point past the last tab when the drag overshoots the strip's
        // right edge.
        let to = target.min(st.shell.tabs.len().saturating_sub(1));
        if to != from {
            st.shell.move_tab(from, to);
        }
    });
    refresh_tab_chrome();
}

/// The View menu's toggles, in tag order.
///
/// A table rather than four selectors so the menu, the tag→setting
/// mapping and the mark refresh all read from one place. `ui_win32` and
/// `ui_gtk` expose the same four; Show Indent Guide has no View-menu
/// entry on those two (it is toolbar-only there), and is included here
/// because this backend has no toolbar yet — leaving it out would make
/// it unreachable rather than merely elsewhere.
pub(crate) const VIEW_TOGGLES: [&str; 4] = [
    "Word Wrap",
    "Show Whitespace",
    "Show End of Line",
    "Show Indent Guide",
];

/// The persisted View settings, or `None` when the state is
/// unreachable. The toolbar's toggle refresh needs the whole struct
/// rather than one flag, because its Show All Characters button stands
/// for two of them.
pub(crate) fn view_settings() -> Option<codepp_core::session::ViewSettings> {
    with_state(|st| st.shell.saved_view_settings())
}

/// Read one View toggle by its tag. Used to paint the menu's check mark.
pub(crate) fn view_setting_by_tag(tag: isize) -> bool {
    with_state(|st| {
        let view = st.shell.saved_view_settings();
        match tag {
            0 => view.word_wrap,
            1 => view.show_whitespace,
            2 => view.show_eol,
            3 => view.indent_guide,
            _ => false,
        }
    })
    .unwrap_or(false)
}

/// Set one View toggle by its tag to an explicit value.
///
/// The toolbar's entry point, as distinct from the menu's
/// [`toggle_view_setting_by_tag`]. A `PushOnPushOff` button has already
/// changed its own state by the time its action fires, so the handler
/// applies that state; a toggle would invert the model against the
/// button and land on the wrong value every other click.
pub(crate) fn set_view_setting(tag: isize, on: bool) {
    with_state(|st| {
        let mut view = st.shell.saved_view_settings();
        match tag {
            0 => view.word_wrap = on,
            1 => view.show_whitespace = on,
            2 => view.show_eol = on,
            3 => view.indent_guide = on,
            _ => return,
        }
        apply_view_settings(&st.editor, view);
        st.shell.set_view_settings(view);
    });
    refresh_toolbar_toggles();
}

/// Flip one View toggle by its tag, apply it to the editor and persist.
///
/// Mutating the whole `ViewSettings` and re-applying it — rather than
/// sending just the one Scintilla message — is deliberate and matches
/// `ui_gtk::toggle_view_setting`: the settings are stored together and
/// applied together, so a partial application is what lets the live
/// editor drift from what the next session save records.
pub(crate) fn toggle_view_setting_by_tag(tag: isize) {
    with_state(|st| {
        let mut view = st.shell.saved_view_settings();
        match tag {
            0 => view.word_wrap = !view.word_wrap,
            1 => view.show_whitespace = !view.show_whitespace,
            2 => view.show_eol = !view.show_eol,
            3 => view.indent_guide = !view.indent_guide,
            _ => return,
        }
        apply_view_settings(&st.editor, view);
        st.shell.set_view_settings(view);
    });
    refresh_toolbar_toggles();
}

/// Set the whitespace and EOL settings together — the toolbar's combined
/// Show All Characters button, matching Win32's.
pub(crate) fn set_show_all_chars(on: bool) {
    with_state(|st| {
        let mut view = st.shell.saved_view_settings();
        view.show_whitespace = on;
        view.show_eol = on;
        apply_view_settings(&st.editor, view);
        st.shell.set_view_settings(view);
    });
    refresh_toolbar_toggles();
}

/// Repaint the toolbar's toggles from the model.
///
/// The single point of agreement between the toolbar and the View menu:
/// every mutation of a View setting ends here, and the menu resolves its
/// own marks on open, so neither surface can show a stale state. `ui_gtk`
/// achieves the same thing with a registry of widget handles and one
/// `refresh_view_indicators`; here the buttons live on the window state,
/// so there is nothing to register.
pub(crate) fn refresh_toolbar_toggles() {
    let toolbar = with_state(|st| st.toolbar.clone());
    if let Some(toolbar) = toolbar {
        toolbar.refresh_toggles();
    }
}

/// Push a `ViewSettings` onto the live editor.
///
/// A straight port of `ui_gtk::apply_view_settings`, including the
/// `SCI_SETSCROLLWIDTH` reset: the tracking-mode high-water mark is
/// shared by the single view and never shrinks on its own, so toggling
/// wrap back off would otherwise leave a phantom horizontal scroll into
/// empty space. Re-seeded through [`seed_horizontal_scroll`] rather than
/// sent raw, because on this backend an unpaired reset collapses the
/// document view to one point wide — see that function.
fn apply_view_settings(editor: &EditorHandle, view: codepp_core::session::ViewSettings) {
    editor.send(
        codepp_scintilla_sys::SCI_SETINDENTATIONGUIDES,
        if view.indent_guide {
            codepp_scintilla_sys::SC_IV_LOOKBOTH
        } else {
            codepp_scintilla_sys::SC_IV_NONE
        },
        0,
    );
    editor.send(
        codepp_scintilla_sys::SCI_SETWRAPMODE,
        if view.word_wrap {
            codepp_scintilla_sys::SC_WRAP_WORD
        } else {
            codepp_scintilla_sys::SC_WRAP_NONE
        },
        0,
    );
    seed_horizontal_scroll(editor);
    editor.send(
        codepp_scintilla_sys::SCI_SETVIEWWS,
        if view.show_whitespace {
            codepp_scintilla_sys::SCWS_VISIBLEALWAYS
        } else {
            codepp_scintilla_sys::SCWS_INVISIBLE
        },
        0,
    );
    editor.send(
        codepp_scintilla_sys::SCI_SETVIEWEOL,
        usize::from(view.show_eol),
        0,
    );
}

/// Push the persisted View settings onto the editor at startup.
///
/// Must run *after* `restore_session`, which is when
/// `saved_view_settings` starts returning the user's stored choices
/// rather than defaults. Without it the editor keeps Scintilla's
/// off-defaults and the first toggle resurfaces every stored setting at
/// once, because [`toggle_view_setting_by_tag`] re-applies the whole
/// struct. `ui_gtk` records the same trap.
pub(crate) fn apply_saved_view_settings() {
    with_state(|st| {
        let view = st.shell.saved_view_settings();
        apply_view_settings(&st.editor, view);
        clamp_scroll_width_to_viewport(&st.editor, &st.sci_view);
    });
    refresh_toolbar_toggles();
}

/// Send one Scintilla command to the active view.
fn editor_cmd(message: u32) {
    with_state(|st| st.editor.send(message, 0, 0));
}

/// Reset the zoom level to 100%.
pub(crate) fn reset_zoom() {
    with_state(|st| st.editor.send(codepp_scintilla_sys::SCI_SETZOOM, 0, 0));
}

/// The active buffer's language id, for the Language menu's radio mark.
pub(crate) fn active_lang_id() -> Option<i32> {
    with_state(|st| st.shell.active().map(|t| t.lang.as_npp_id())).flatten()
}

/// The active buffer's id, for the Window menu's mark.
pub(crate) fn active_tab_id() -> Option<i32> {
    with_state(|st| st.shell.active().map(|t| t.id)).flatten()
}

/// Apply a language to the active buffer, re-lex, and repaint the status
/// bar. A no-op if the buffer already has it.
pub(crate) fn apply_language(lang_id: i32) {
    let lang = codepp_core::LangType(lang_id);
    if with_state(|st| st.shell.set_active_lang(lang)) != Some(true) {
        return;
    }
    with_state(|st| {
        let (shell, mut ui) = st.split();
        ui.apply_lang(lang);
        if let Some(tab) = shell.active() {
            let (l, enc, eol, len) = (tab.lang, tab.encoding.clone(), tab.eol, tab.byte_len);
            ui.update_status(l, &enc, eol, len);
        }
    });
}

/// The Encoding menu tag matching the active buffer's encoding, or
/// `None` when it is one this menu does not list (a codepage, or a
/// BOM-less UTF-16). Those simply show no mark rather than being
/// misreported as one of the five rows.
pub(crate) fn active_encoding_tag() -> Option<isize> {
    let active = with_state(|st| st.shell.active().map(|t| t.encoding.clone())).flatten()?;
    match active {
        codepp_core::Encoding::Utf8 => Some(1),
        codepp_core::Encoding::Utf8Bom => Some(2),
        // The no-BOM UTF-16 variants mark their BOM row. They are what
        // `core::encoding`'s zero-byte heuristic produces for a real
        // un-BOM'd UTF-16 file, and both other backends deliberately
        // treat them as the same family — `ui_gtk::same_encoding_family`
        // and `ui_win32::refresh_encoding_menu` each say so in as many
        // words. Falling through to `None` instead would leave the menu
        // showing no selection at all for a perfectly ordinary file.
        codepp_core::Encoding::Utf16LeBom | codepp_core::Encoding::Utf16Le => Some(3),
        codepp_core::Encoding::Utf16BeBom | codepp_core::Encoding::Utf16Be => Some(4),
        // A codepage. Genuinely not one of the five rows, so no mark.
        // Named rather than wildcarded so a new `Encoding` variant is a
        // compile error here — the two above are exactly the case a
        // wildcard would have swallowed silently.
        codepp_core::Encoding::Other(_) => None,
    }
}

/// Apply the save encoding chosen from the Encoding menu.
pub(crate) fn apply_encoding_by_tag(tag: isize) {
    let encoding = match tag {
        1 => codepp_core::Encoding::Utf8,
        2 => codepp_core::Encoding::Utf8Bom,
        3 => codepp_core::Encoding::Utf16LeBom,
        4 => codepp_core::Encoding::Utf16BeBom,
        // Tag 0 is the disabled ANSI row, which cannot be chosen.
        _ => return,
    };
    if with_state(|st| st.shell.set_buffer_encoding(encoding)) == Some(true) {
        // `refresh_title` as well as the status bar, unlike
        // `ui_gtk::refresh_active_status`: on this backend the window
        // title is rebuilt from the tab rather than tracked, and changing
        // the save encoding can mark the buffer dirty.
        with_state(|st| {
            let (shell, mut ui) = st.split();
            if let Some(tab) = shell.active() {
                let (l, enc, eol, len) = (tab.lang, tab.encoding.clone(), tab.eol, tab.byte_len);
                ui.update_status(l, &enc, eol, len);
            }
        });
        refresh_title();
    }
}

/// Every open buffer as `(id, display name)`, in tab order — the Window
/// menu's rows.
///
/// `None` — not an empty `Vec` — when the state is unreachable, so the
/// caller can leave the menu as it is rather than rebuilding it into
/// "no files open". The distinction is the same one `active_dirty`
/// draws, and it matters for the same reason: "could not look" is not
/// "nothing there".
pub(crate) fn open_buffer_rows() -> Option<Vec<(i32, String)>> {
    with_state(|st| {
        st.shell
            .tabs
            .iter()
            .map(|t| (t.id, codepp_shell::tab_display_name(t)))
            .collect()
    })
}

/// File → Close All.
///
/// Loops the single-tab close so each dirty buffer gets its own
/// Save / Don't Save / Cancel prompt and a Cancel stops the rest,
/// matching `ui_gtk::on_close_all` and Win32's
/// `close_multiple_documents`.
pub(crate) fn action_close_all() {
    loop {
        let before = with_state(|st| st.shell.tabs.len()).unwrap_or(0);
        if before == 0 {
            break;
        }
        if !action_close_tab() {
            break;
        }
        // **The termination condition, and it is not optional.**
        // `action_close_tab` reseeds a fresh untitled buffer when it
        // closes the last one, so the list never actually reaches zero —
        // and that reseeded buffer is clean, so the close gate waves it
        // through without a prompt and the loop would close-and-recreate
        // forever, pinning a core with no visible cause. Ending on "the
        // close made no progress" is what leaves the workspace at one
        // empty untitled buffer, which is what Notepad++ does and what
        // `ui_gtk::on_close_all` carries the same guard to achieve.
        if with_state(|st| st.shell.tabs.len()).unwrap_or(0) >= before {
            break;
        }
    }
}

/// View → Restore Default Window Size.
///
/// A recovery action for a window left in an awkward state — dragged
/// mostly off-screen, or restored onto a display that has since changed.
/// Same entry `ui_gtk` carries, for the same reason.
pub(crate) fn restore_default_window_size() {
    // The window is taken *out* of the borrow before being resized.
    // `windowDidResize:` fires synchronously from `setContentSize`, and
    // it calls `refresh_tab_chrome`, which needs `with_state` — so
    // resizing from inside a borrow makes that refresh a no-op and leaves
    // the tab strip laid out for the old width, arrows and all.
    let Some(window) = with_state(|st| st.window.clone()) else {
        return;
    };
    if window.isZoomed() {
        window.zoom(None);
    }
    window.setContentSize(NSSize::new(DEFAULT_WIDTH, DEFAULT_HEIGHT));
    window.center();
}

/// Open a path that was dropped onto the window.
pub(crate) fn open_dropped_path(path: PathBuf) {
    open_path(path);
}

/// Whether the bound Scintilla document holds nothing the user could
/// miss: no text, and no undo or redo history.
///
/// The editor half of `Shell::open_file_replacing_scratch`'s decision —
/// see that method for why the shell cannot answer it. Emptiness alone
/// is not enough: typing a character and deleting it leaves the document
/// empty *and* unmodified, because undoing back past the save point
/// clears Scintilla's modify bit. The undo history is what still
/// remembers it happened, and a buffer the user has been working in must
/// not be silently replaced by an Open.
///
/// **Redo is checked as well as undo, and that is not belt-and-braces —
/// it is the only thing covering the case.** Measured by driving the
/// app: after typing one character into the startup buffer and undoing
/// it, Scintilla reports `SCI_GETLENGTH == 0`, `SCI_CANUNDO == 0` and
/// `SCI_CANREDO == 1`. Every clause but the redo one says "untouched",
/// so a check written against length and undo alone would discard a
/// buffer the user had been working in.
fn editor_is_pristine(editor: &codepp_editor::EditorHandle) -> bool {
    editor.send(codepp_scintilla_sys::SCI_GETLENGTH, 0, 0) == 0
        && editor.send(codepp_scintilla_sys::SCI_CANUNDO, 0, 0) == 0
        && editor.send(codepp_scintilla_sys::SCI_CANREDO, 0, 0) == 0
}

/// Open `path`, rebinding the view when the shell only switched tabs.
///
/// **The return value is load-bearing.** A fresh open queues an async
/// load, and the view is rebound when that load lands through
/// `drain_shell`. But if the path is *already open*, the shell just
/// flips `active_tab` and no load — and therefore no wake — ever fires,
/// so nothing rebinds the single Scintilla view. `OpenFileOutcome`'s own
/// documentation says exactly this: the UI must issue a synchronous
/// rebind "otherwise the tab bar shows the switch while the editor keeps
/// rendering the previous buffer".
///
/// Ignoring it is what made the editor appear to stop responding to the
/// mouse after opening a file: the strip showed the new tab, the view
/// still held the old (often empty) document, and clicks moved a caret
/// nobody could see. `ui_gtk` has always handled this; the Cocoa port
/// did not.
fn open_path(path: PathBuf) {
    let outcome = with_state(|st| {
        st.shell
            .open_file_replacing_scratch(path, editor_is_pristine(&st.editor))
    });
    if let Some(codepp_shell::OpenFileOutcome::SwitchedToExisting(_)) = outcome {
        rebind_active_view();
    } else {
        refresh_tab_chrome();
    }
}

/// Open the recent-files entry at `index`, removing it from the list —
/// it is open now, and re-enters when it is next closed.
///
/// `index` is captured when the File menu is rebuilt on open, and an
/// index rather than an id is safe here in a way it would not be for a
/// tab (see §7.4's arm/commit race, which is why *those* key on
/// `Tab.id`). Two things make it so, and the second is the one that
/// could stop holding: menu tracking is a synchronous main-thread
/// session, and nothing in the workspace mutates `Shell.recent_files`
/// off that thread — no worker, timer or plugin path reaches it, only
/// a tab close and these four commands. `take_recent_file_at`
/// bounds-checks regardless, so a stale index is a no-op rather than a
/// wrong file; and `rebuild_recent_region` clears its rows rather than
/// leaving stale ones when it cannot read the list at all.
pub(crate) fn open_recent_at(index: usize) {
    let path = with_state(|st| st.shell.take_recent_file_at(index)).flatten();
    if let Some(path) = path {
        open_path(path);
    }
}

/// ⇧⌘T / Restore Recent Closed File: reopen the most recently closed.
pub(crate) fn restore_recent_closed() {
    let path = with_state(|st| st.shell.pop_last_recent_file()).flatten();
    if let Some(path) = path {
        open_path(path);
    }
}

/// Open every recent file, most-recent first, emptying the list.
pub(crate) fn open_all_recent() {
    let paths = with_state(|st| st.shell.take_all_recent_files()).unwrap_or_default();
    for path in paths {
        open_path(path);
    }
}

/// Drop every tracked recent path.
pub(crate) fn empty_recent_files() {
    with_state(|st| st.shell.clear_recent_files());
}

/// Whether anything is in the recent-files list, for the enabled state
/// of the three items that act on it.
///
/// A declined borrow answers `false` — i.e. greys the item — because the
/// alternative is offering a command that will find nothing to do.
pub(crate) fn has_recent_files() -> bool {
    with_state(|st| !st.shell.visible_recent_files().is_empty()).unwrap_or(false)
}

/// The recent-files rows for the File menu: one already-numbered,
/// already-sanitized label per entry (`"1: notes.txt"`), plus the config
/// that shapes the region. The index is the row's position, recovered by
/// the caller with `enumerate` and carried in each item's tag. `None` when the state borrow is declined — which the caller
/// must not confuse with "no recent files", for the same reason
/// [`open_buffer_rows`] draws that distinction.
pub(crate) fn recent_file_rows() -> Option<(Vec<String>, RecentFilesHistoryConfig)> {
    with_state(|st| {
        let cfg = st.shell.preferences.recent_files_history.clone();
        let rows = st
            .shell
            .visible_recent_files()
            .iter()
            .enumerate()
            // A path is attacker-influenced display text, so it goes
            // through the same sanitizer filenames take everywhere else
            // in this backend. The *functional* value is the index,
            // which is carried in the item's tag and never parsed back
            // out of this string.
            .map(|(i, p)| {
                format!(
                    "{}: {}",
                    i + 1,
                    codepp_shell::sanitize_str_for_display(&cfg.display_path(p))
                )
            })
            .collect();
        (rows, cfg)
    })
}

/// Restore the previous session's buffers, then any path from the
/// command line.
fn restore_session(initial_path: Option<PathBuf>) {
    let entries = with_state(|st| st.shell.load_session_entries()).unwrap_or_default();
    for entry in entries {
        match entry {
            SessionRestoreEntry::OpenFile(path) => {
                // Each normally queues an async load whose completion
                // rebinds the view through `drain_shell` — but a
                // duplicate path in session.xml dedupes to
                // `SwitchedToExisting`, which has no load to wake. See
                // `open_path`.
                open_path(path);
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
        open_path(path);
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
    // The map's own visibility and width live in `docmap`'s
    // thread-locals, not in the shell, so they have to be pushed across
    // before the save reads the session. Same shape as the geometry sync
    // above and as `ui_gtk`'s `docmap::sync_to_shell`.
    docmap::sync_to_shell();
    workspace::sync_to_shell();
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
    // `refresh_tab_chrome` re-seeds the horizontal scroll range, which is
    // what stops a brand-new empty buffer inheriting the previous file's
    // — see `seed_horizontal_scroll_if_document_changed`.
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
            // Same rebind requirement as every other open path — see
            // `open_path`.
            open_path(path);
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
    // The whole chrome, not just the title. The save clears Scintilla's
    // save point from inside the `with_state` borrow above, so the
    // resulting `SCN_SAVEPOINTREACHED` re-enters `on_sci_notify` while
    // that borrow is live and is declined — leaving the tab's dirty
    // marker showing unsaved changes for a buffer that is now on disk.
    // Same obligation, and the same fix, as `search::refresh_chrome`.
    refresh_tab_chrome();
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
    // See `action_save_file` — the save point is cleared under a borrow,
    // so the dirty marker will not clear itself. Save As additionally
    // renames the tab, which the strip rebuild here picks up.
    refresh_tab_chrome();
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
    // `save_all` already clears the cached `Tab.dirty` for each tab it
    // saves — it is authoritative precisely because its
    // `SCN_SAVEPOINTREACHED` notifications land inside its own borrow
    // and every backend declines them. What is still missing is the
    // *repaint*: nothing has redrawn the strip against those cleared
    // flags. That is what this call is for.
    refresh_tab_chrome();
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
        // not the cached flag alone. Before m3b wired Scintilla's
        // notifications, nothing on this backend set `Tab.dirty` in
        // response to typing at all, which made this whole gate inert:
        // every ordinary typed edit reported clean and closed without a
        // prompt. The OR still earns its place now that they are wired,
        // because a notification can be missed when it arrives inside an
        // outer `with_state` borrow (see `on_sci_notify`). Same OR
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
            // Re-read the **live** state, not the cached `Tab.dirty`.
            //
            // The save clears Scintilla's save point from inside a
            // `with_state` borrow (`save_current_to_disk` calls
            // `ui.mark_saved()` while `Shell::split` holds it), so the
            // resulting `SCN_SAVEPOINTREACHED` re-enters `on_sci_notify`
            // with the borrow live and is declined — exactly the
            // obligation that function's docs state. Reading the cached
            // bit therefore saw "still dirty" for a buffer that had just
            // reached disk, and refused the close the user had already
            // confirmed: Save wrote the file and left the tab open.
            //
            // `active_dirty()` is the live modify bit ORed with
            // `is_unsaved_restore`, which is the right pair here: a
            // successful save also clears the restore id, so a recovered
            // buffer closes once it has a real copy on disk and not
            // before. `ui_gtk` re-reads `SCI_GETMODIFY` at this point for
            // the same reason, without the restore half.
            //
            // `None` — the state was unreachable — deliberately aborts
            // the close rather than assuming clean.
            active_dirty() == Some(false)
        }
        1002 => true, // Don't Save
        _ => false,   // Cancel, or the window closed under the alert
    }
}

/// Close the active tab, gated on the Save / Don't Save / Cancel
/// prompt.
///
/// Returns `false` when the user cancelled, so a caller closing several
/// buffers in a row stops rather than prompting for every remaining one
/// — that is what File → Close All relies on, and it matches
/// `ui_gtk::close_active_tab`'s contract.
pub(crate) fn action_close_tab() -> bool {
    let closed = confirm_discard_active();
    if closed {
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
    closed
}

pub(crate) fn action_reload() {
    with_state(|st| st.shell.reload_active());
}

/// Convert an `NSURL` from a panel into a filesystem path.
///
/// **Gated on the URL actually being a file URL**, for the reason
/// `dropview::dragged_paths` documents at the drag-and-drop boundary:
/// `-[NSURL path]` returns the *path component* of any hierarchical URL
/// rather than nil, so `http://evil/etc/passwd` yields `/etc/passwd` and
/// would reach `Shell::open_file` as though the user had picked a local
/// file. `ui_gtk` rejects non-`file:` schemes at the same boundary in its
/// `uri_to_local_path`.
///
/// An `NSOpenPanel`/`NSSavePanel` browsing the local filesystem is far
/// less steerable than a pasteboard, so this is defence in depth rather
/// than a live hole — but the guard existed on one of the two Cocoa
/// entry points and not the other, which is exactly how the three
/// incidents DESIGN.md §7.4 records got in.
pub(crate) fn url_to_path(url: &NSURL) -> Option<PathBuf> {
    if !url.isFileURL() {
        return None;
    }
    url.path().map(|p| PathBuf::from(p.to_string()))
}

#[cfg(test)]
mod scroller_layout_tests {
    use super::scroller_clip_size;

    #[test]
    fn each_scroller_and_the_margin_take_their_own_space() {
        // The measured real case: a 1080x548 scroll view with a 44pt
        // line-number gutter and both 17pt bars showing.
        assert_eq!(
            scroller_clip_size(1080.0, 548.0, 44.0, 17.0, 17.0),
            (1019.0, 531.0)
        );
    }

    #[test]
    fn an_absent_scroller_gives_its_space_back() {
        assert_eq!(
            scroller_clip_size(1080.0, 548.0, 44.0, 0.0, 17.0),
            (1036.0, 531.0)
        );
        assert_eq!(
            scroller_clip_size(1080.0, 548.0, 44.0, 17.0, 0.0),
            (1019.0, 548.0)
        );
    }

    #[test]
    fn a_buffer_with_no_gutter_uses_the_full_width() {
        assert_eq!(
            scroller_clip_size(1080.0, 548.0, 0.0, 17.0, 17.0),
            (1063.0, 531.0)
        );
    }

    #[test]
    fn the_axes_are_not_swapped() {
        // A deliberately asymmetric case: were the two subtractions
        // transposed, this would come back (983, 611).
        assert_eq!(
            scroller_clip_size(1000.0, 600.0, 10.0, 7.0, 11.0),
            (983.0, 589.0)
        );
    }

    #[test]
    fn a_window_smaller_than_its_own_chrome_floors_at_zero() {
        assert_eq!(scroller_clip_size(20.0, 10.0, 44.0, 17.0, 17.0), (0.0, 0.0));
    }
}

#[cfg(test)]
mod modifier_tests {
    use super::modifiers_type_a_character as types;
    use objc2_app_kit::NSEventModifierFlags as F;

    #[test]
    fn a_plain_key_is_a_typed_character() {
        assert!(types(F::empty()));
        assert!(types(F::Shift));
        assert!(types(F::CapsLock));
    }

    /// ⌥ composes real characters (⌥e, ⌥u), so it counts — the macOS
    /// analogue of the `AltGr` case `ui_gtk` reasons about.
    #[test]
    fn option_still_types_a_character() {
        assert!(types(F::Option));
        assert!(types(F::Option | F::Shift));
    }

    /// ⌘ and ⌃ are commands. §8 budgets a *typed character*, and a
    /// paste's redraw is a different quantity large enough to dominate
    /// the tail.
    #[test]
    fn command_and_control_are_chords() {
        assert!(!types(F::Command));
        assert!(!types(F::Control));
        assert!(!types(F::Command | F::Shift));
        // ⌥ does not rescue a chord, unlike GTK's Ctrl+Alt.
        assert!(!types(F::Control | F::Option));
    }
}

#[cfg(test)]
mod source_invariants {
    //! Source-level guards for invariants a runtime test cannot see.
    //!
    //! Each of these encodes a bug that actually shipped and was found
    //! by a user rather than by the suite. They are source scans because
    //! every one of them fails by *absence* — a call that is not made, a
    //! pairing that is broken — and needs a real window server, a real
    //! run loop, or a human looking at pixels to observe directly.
    //!
    //! ## Window activation
    //!
    //! Ordering the window front and activating the application must
    //! run from the application delegate's
    //! `applicationDidFinishLaunching:`, never from [`run`] before
    //! `-[NSApplication run]`. Doing it early silently fails for a
    //! process outside an `.app` bundle: the window never becomes key,
    //! the app never becomes active, and the window server stops routing
    //! mouse clicks to it — while keyboard input still appears to work,
    //! so the symptom reads as "the mouse is broken".
    //!
    //! **Why a source scan and not a runtime test.** The failure only
    //! manifests with a real run loop and a real window server, and it
    //! manifests as *absence* — no click arrives. A headless test cannot
    //! observe it, and the display-gated `cocoa_smoke` test has no run
    //! loop either (it is `harness = false` precisely so it owns the
    //! main thread, not so it pumps events). This shipped once and was
    //! found by a user, not by the suite; a scan is the cheapest thing
    //! that would have caught it. Same tool `ui_gtk` uses for its
    //! single-view invariant, and for the same reason.

    /// The crate source with its own test module removed.
    ///
    /// `include_str!` embeds this whole file, tests included — so a
    /// pattern quoted inside an assertion message would count as a real
    /// call site and the scan would flag itself. Cutting at the first
    /// `#[cfg(test)]` is the same trick `ui_gtk`'s source scanner uses.
    fn production_src() -> &'static str {
        let src = include_str!("lib.rs");
        match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// The body of `fn name`, by brace matching from its signature.
    fn fn_body(src: &str, name: &str) -> String {
        let sig = format!("fn {name}(");
        let start = src.find(&sig).unwrap_or_else(|| panic!("no fn {name}"));
        let open = src[start..].find('{').expect("no body") + start;
        let mut depth = 0usize;
        for (i, c) in src[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return src[open..=open + i].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated body for {name}");
    }

    #[test]
    fn activation_lives_in_the_delegate_not_in_run() {
        let src = production_src();
        // Sanity: a broken read must not let this pass vacuously.
        assert!(src.len() > 5_000, "source scan read too little to be real");

        let run_body = fn_body(src, "run");
        for forbidden in ["makeKeyAndOrderFront", "makeFirstResponder", ".activate("] {
            assert!(
                !run_body.contains(forbidden),
                "`{forbidden}` is back inside `run()`. Activation there is \
                 silently ignored for a non-bundled process, which kills \
                 mouse input while leaving the keyboard working. Move it \
                 to `activate_main_window`, called from the delegate's \
                 `applicationDidFinishLaunching:`."
            );
        }

        let activate_body = fn_body(src, "activate_main_window");
        for required in ["makeKeyAndOrderFront", "makeFirstResponder", ".activate("] {
            assert!(
                activate_body.contains(required),
                "`activate_main_window` no longer calls `{required}` — the \
                 window would never become key."
            );
        }
    }

    /// Cold start must close on a real paint, and nowhere else.
    ///
    /// This one is a regression guard for a bug that shipped and stood
    /// for five milestones: `mark_first_draw` used to be called from
    /// `run()`, before `-[NSApplication run]` — so before the window was
    /// ordered front (which moved into the delegate in m3b), before the
    /// app was activated, and before anything painted. Every macOS
    /// cold-start figure §8 recorded up to m4b was therefore measuring a
    /// different endpoint than `ui_gtk` and `ui_win32`, both of which
    /// close on `SCN_PAINTED`, and under-reporting by 60–95 ms.
    ///
    /// A runtime test cannot catch this: the wrong mark produces a
    /// perfectly plausible number, just of the wrong quantity. Nothing
    /// fails, nothing looks odd — which is precisely why it survived so
    /// long. Same tool, and same reasoning, as the activation guard
    /// above.
    #[test]
    fn cold_start_is_marked_at_the_paint_and_not_in_run() {
        let src = production_src();
        assert!(src.len() > 5_000, "source scan read too little to be real");

        assert!(
            !fn_body(src, "run").contains("mark_first_draw"),
            "`mark_first_draw` is back inside `run()`, which returns \
             before `-[NSApplication run]` — so it would fire before the \
             window is shown and before anything paints, measuring a \
             different span than `ui_gtk` and `ui_win32`. It belongs in \
             the `SCN_PAINTED` arm of `on_sci_notify_inner`."
        );

        // And in the paint arm it must precede the arm's `with_state`:
        // that borrow is declined re-entrantly, and a mark behind it
        // would be skipped on exactly the paints that re-enter.
        let arm = fn_body(src, "on_sci_notify_inner");
        let paint = arm
            .find("SCN_PAINTED =>")
            .expect("no SCN_PAINTED arm in on_sci_notify_inner");
        let tail = &arm[paint..];
        let mark = tail
            .find("mark_first_draw()")
            .expect("the SCN_PAINTED arm no longer marks cold start");
        let borrow = tail.find("with_state(").unwrap_or(usize::MAX);
        assert!(
            mark < borrow,
            "`mark_first_draw` must run before the paint arm's \
             `with_state`, which is declined on a re-entrant call and \
             would silently drop the timestamp."
        );
    }

    /// The main window must be pinned to sRGB, before it is shown.
    ///
    /// The pin is what keeps the window's compositing surfaces at 4
    /// bytes per pixel instead of the half-float 8 `CoreAnimation`
    /// otherwise picks on a wide-gamut display — worth ~27 MB, a third
    /// of this backend's footprint (§8). Dropping the call costs that
    /// silently: nothing fails, nothing renders differently, and the
    /// only symptom is a number in a document nobody re-measures every
    /// commit. That is precisely the shape of bug the cold-start mark
    /// turned out to be, so it gets the same guard.
    ///
    /// Placement matters as much as presence — it has to precede
    /// `build_content`, which is what installs the content view, since
    /// the point is to be set before any backing store is allocated.
    #[test]
    fn the_window_is_pinned_to_srgb_before_it_is_shown() {
        let src = production_src();
        let body = fn_body(src, "run");
        let pin = body.find("setColorSpace(").expect(
            "the main window is no longer pinned to sRGB — its \
             compositing surfaces double in size (see the call site)",
        );
        let content = body
            .find("build_content(")
            .expect("`run` no longer builds the content view");
        assert!(
            pin < content,
            "`setColorSpace` must run before `build_content`, which \
             installs the content view — the pin has to be in place \
             before any backing store is allocated."
        );
    }

    /// Every open must go through `open_path`, which handles
    /// `OpenFileOutcome::SwitchedToExisting`.
    ///
    /// A fresh open queues an async load and the view is rebound when it
    /// lands. Re-opening an *already open* path only flips `active_tab`
    /// — no load, no wake, no rebind — so a caller that ignores the
    /// return value leaves the strip showing the new tab while the view
    /// keeps rendering the previous document. `OpenFileOutcome`'s own
    /// documentation states this requirement; ignoring it shipped as
    /// "the mouse stops working after opening a file", because clicks
    /// were landing in a stale (often empty) buffer.
    ///
    /// Guarded by a source scan for the same reason as the activation
    /// invariant above: it needs a real window and a real document swap
    /// to observe, and it fails by *absence* of a rebind.
    #[test]
    fn opens_go_through_the_rebinding_helper() {
        let src = production_src();
        assert!(src.len() > 5_000, "source scan read too little to be real");
        // Both entry points, so a future caller reaching for the plain
        // `open_file` is caught as readily as one reaching for the
        // scratch-replacing form.
        let calls = src.matches(".open_file(").count()
            + src.matches(".open_file_replacing_scratch(").count();
        assert_eq!(
            calls, 1,
            "a `Shell` open call should appear exactly once, inside \
             `open_path` — every other caller must go through it so \
             `SwitchedToExisting` still rebinds the view. Found {calls}."
        );
        let open_path_body = fn_body(src, "open_path");
        assert!(
            open_path_body.contains("SwitchedToExisting"),
            "`open_path` no longer handles `SwitchedToExisting`; \
             re-opening an already-open file would leave the editor on \
             the previous document."
        );
        assert!(
            open_path_body.contains("rebind_active_view"),
            "`open_path` no longer rebinds the view."
        );
        // And it must actually *measure* the editor rather than passing
        // a literal. Hard-coding `false` here would silently restore the
        // old behaviour — the startup `new 1` left stranded beside the
        // file the user opened — with nothing failing to say so.
        assert!(
            open_path_body.contains("editor_is_pristine("),
            "`open_path` no longer measures the editor, so the untouched \
             `new 1` will not be replaced by an opened file."
        );
    }

    /// `SCI_SETSCROLLWIDTH` must never be sent without
    /// `SCI_SETSCROLLWIDTHTRACKING` beside it.
    ///
    /// On Cocoa `scrollWidth` directly sizes the `NSScrollView`'s
    /// document view. Resetting it to 1 with nothing to recompute it
    /// collapses `SCIContentView` to one point wide, hit-testing falls
    /// through to the enclosing `NSClipView`, and Scintilla stops
    /// receiving `mouseDown:` entirely — the editor appears to ignore
    /// the mouse while the keyboard still works, because keyboard input
    /// goes to the first responder rather than through hit-testing.
    /// That shipped once, reported as "the mouse stops working after
    /// switching tabs".
    ///
    /// There are exactly two legitimate senders, and both are named
    /// here: `seed_horizontal_scroll` (which pairs the reset with
    /// tracking) and `clamp_scroll_width_to_viewport` (which raises the
    /// floor so the whole visible area stays clickable). A third call
    /// site is almost certainly one of the two bugs above coming back.
    #[test]
    fn scroll_width_is_only_set_together_with_tracking() {
        let src = production_src();
        assert!(src.len() > 5_000, "source scan read too little to be real");
        let sets = src.matches("SCI_SETSCROLLWIDTH,").count();
        assert_eq!(
            sets, 3,
            "`SCI_SETSCROLLWIDTH` should be sent from exactly three places \
             — `seed_horizontal_scroll`, which also enables tracking, and \
             the two arms of `clamp_scroll_width_to_viewport`, which raise \
             the floor to the viewport and lower it again when the viewport \
             shrinks. Found {sets}. An unpaired reset collapses the document \
             view to one point wide and kills mouse input entirely; a \
             missing floor makes only the text clickable."
        );
        // The count is a proxy; this is the property that actually
        // matters. Only the seed may reset the width to 1, because that
        // is the value that collapses the document view — and it is the
        // one place that re-enables tracking alongside.
        let seed_body = fn_body(src, "seed_horizontal_scroll");
        let resets = src.matches("SCI_SETSCROLLWIDTH, 1,").count();
        assert_eq!(
            resets,
            seed_body.matches("SCI_SETSCROLLWIDTH, 1,").count(),
            "something outside `seed_horizontal_scroll` resets the scroll \
             width to 1; only the seed may, because only it re-enables \
             tracking in the same breath"
        );
        // The *callers* matter as much as the senders, and this is the
        // one that bites. Seeding at the document swap — inside
        // `UiPlatform::activate_tab` in `platform.rs` — reads like the
        // tighter place for it and resets the horizontal scroll position
        // of a buffer the user never left, because `Shell::save_all`
        // binds every titled tab in turn as bookkeeping. Measured before
        // this guard existed: a buffer scrolled to x=1500 came back to 0
        // after File → Save All. Both helpers must therefore be reached
        // only from `seed_horizontal_scroll_if_document_changed`, which
        // runs once the binding has settled.
        let platform_src = include_str!("platform.rs");
        for helper in ["seed_horizontal_scroll(", "clamp_scroll_width_to_viewport("] {
            assert!(
                !platform_src.contains(helper),
                "`platform.rs` calls `{helper}` — almost certainly from \
                 `activate_tab`, which fires on `save_all`'s bookkeeping \
                 swaps too and would reset the scroll position of the \
                 buffer the user is looking at. Seed from \
                 `seed_horizontal_scroll_if_document_changed` instead."
            );
        }
        let settled = fn_body(src, "seed_horizontal_scroll_if_document_changed");
        assert!(
            settled.contains("seed_horizontal_scroll(")
                && settled.contains("clamp_scroll_width_to_viewport(")
                && settled.contains("SCI_GETDOCPOINTER"),
            "the settled-binding seed no longer keys on the bound \
             document, so it either seeds on every chrome refresh or not \
             at all"
        );

        let clamp = fn_body(src, "clamp_scroll_width_to_viewport");
        assert!(
            clamp.contains("SCI_SETSCROLLWIDTH,") && clamp.contains("SCI_GETSCROLLWIDTH"),
            "`clamp_scroll_width_to_viewport` no longer raises the floor \
             — clicks to the right of a line's text would stop reaching \
             the editor."
        );
        let body = fn_body(src, "seed_horizontal_scroll");
        assert!(
            body.contains("SCI_SETSCROLLWIDTH,"),
            "`seed_horizontal_scroll` no longer seeds the width"
        );
        assert!(
            body.contains("SCI_SETSCROLLWIDTHTRACKING"),
            "`seed_horizontal_scroll` no longer enables width tracking — \
             the reset alone pins the document view to one point wide."
        );
    }

    /// The shared editor-styling helpers must all be applied.
    ///
    /// `apply_predefined_styles` is this backend's copy of a list every
    /// backend runs, and omissions there are invisible in logic tests:
    /// the feature still works, it just renders with Scintilla's
    /// defaults instead of Code++'s palette. Missing
    /// `configure_change_history_margin` shipped exactly that way — the
    /// change-history strip drew as an outlined bar with a lighter fill
    /// on macOS while Win32 and GTK drew a solid orange one, and only a
    /// user comparing the two platforms could tell.
    #[test]
    fn apply_predefined_styles_runs_the_shared_helpers() {
        let src = include_str!("platform.rs");
        assert!(src.len() > 5_000, "source scan read too little to be real");
        let body = fn_body(src, "apply_predefined_styles");
        for required in [
            "apply_line_number_margin",
            "enable_line_number_margin",
            "configure_change_history_margin",
            "apply_brace_styles",
            "apply_indent_guide_style",
        ] {
            assert!(
                body.contains(required),
                "`apply_predefined_styles` no longer calls `{required}`. \
                 The other backends do; dropping it does not break the \
                 feature, it just renders it differently on macOS."
            );
        }
    }

    /// Every tab switch must record the outgoing buffer's dirty state
    /// **before** it moves `active_tab`.
    ///
    /// Reversed, the read still succeeds — it just attributes the
    /// outgoing document's modify bit to the incoming tab, so the wrong
    /// tab wears the unsaved-changes marker. Nothing crashes and nothing
    /// logs, which is why this is a scan: the failure is a wrong pixel
    /// in a strip that is rebuilt constantly, and no assertion in a
    /// headless test can see it.
    #[test]
    fn dirty_is_captured_before_the_active_tab_moves() {
        let src = production_src();
        for name in ["select_tab_by_id", "close_tab_by_id"] {
            let body = fn_body(src, name);
            let capture = body
                .find("capture_active_dirty()")
                .unwrap_or_else(|| panic!("{name} never captures the outgoing dirty state"));
            let write = body
                .find("active_tab = Some(idx)")
                .unwrap_or_else(|| panic!("{name} no longer writes active_tab as expected"));
            assert!(
                capture < write,
                "{name} moves active_tab before capturing the outgoing dirty state, \
                 so the incoming tab inherits the outgoing buffer's marker"
            );
        }
    }

    /// The dirty marker must consult `Shell::is_unsaved_restore`, not
    /// `SCI_GETMODIFY` alone.
    ///
    /// A buffer restored from a crash-recovery backup sits at its
    /// Scintilla save point with no copy on disk, so the live modify bit
    /// reads clean for a buffer whose contents exist only in memory.
    /// DESIGN.md §7.4 records this as an open Win32 bug; the scan is
    /// what stops the Cocoa backend drifting back into it.
    ///
    /// **What this does and does not catch.** It is a presence check:
    /// mutation-verified against *deleting* the consultation, which is
    /// the realistic regression (a future refactor simplifying the
    /// helper down to the raw bit). It cannot see the call being made
    /// and its result then discarded — no source scan can. A runtime
    /// test would need a live `CocoaUiState`, i.e. a window server and a
    /// real `Shell`, which is why the second assertion below exists
    /// instead: keeping the number of live-modify-bit readers pinned at
    /// two is what stops a fourth site quietly growing its own,
    /// unrestored-aware copy of this logic.
    #[test]
    fn the_dirty_poll_accounts_for_restored_buffers() {
        let body = fn_body(production_src(), "active_dirty");
        // The call form, not the bare identifier: the body carries a
        // comment naming `is_unsaved_restore` in prose, and matching that
        // made the guard pass against a mutation that removed the call.
        assert!(
            body.contains(".is_unsaved_restore("),
            "active_dirty no longer ORs is_unsaved_restore, so a restored \
             buffer paints clean after an edit-then-undo"
        );
        // And it must be the *only* place the raw bit is turned into a
        // marker, so there is one thing to keep correct.
        let src = production_src();
        // Call sites only — the pattern includes `send(` so the prose in
        // the doc comments above does not count as one.
        let raw = src
            .matches("send(codepp_scintilla_sys::SCI_GETMODIFY")
            .count();
        assert_eq!(
            raw, 2,
            "expected exactly two live-modify-bit reads in lib.rs \
             (active_dirty, and confirm_discard_active's independent close \
             gate); found {raw}. A third caller almost certainly wants \
             active_dirty() instead."
        );
    }

    /// `TabButton::track` must not touch `self` after it mutates the
    /// model.
    ///
    /// Its commit path resyncs the tab strip, which sends
    /// `removeFromSuperview` to every control in it — including the
    /// button whose `mouseDown:` is still on the stack. The superview
    /// holds the only strong reference, and Objective-C dispatch does not
    /// retain the receiver, so a `self.` after that point is a
    /// use-after-free rather than a stale read. The loop therefore
    /// decides the gesture and acts on it only after `drop(freeze)`.
    ///
    /// A scan because the failure is a dangling pointer inside AppKit,
    /// reachable only from a real drag on a real window server, and
    /// because it would most likely *not* crash in a debug build — the
    /// freed memory is still mapped and usually still intact.
    /// Forcing a Legacy scroller style obliges us to repair Scintilla's
    /// tiling, on every paint.
    ///
    /// `SCIScrollView::tile` shifts the content view right by the
    /// line-number margin's width without accounting for scrollers that
    /// take space rather than float, and
    /// `ScintillaCocoa::SetScrollingSize` re-tiles whenever the document
    /// resizes — so a long line silently left the content view covering
    /// both scrollers and they vanished. That shipped, and reverting the
    /// style shipped too and left no bars at all.
    ///
    /// A scan because the failure is a layout interaction inside
    /// vendored Objective-C++ that only appears once a document is wider
    /// than the viewport: nothing in the suite can see it, and both
    /// previous attempts needed a user to find them.
    #[test]
    fn a_forced_scroller_style_comes_with_its_layout_repair() {
        let src = production_src();
        if !src.contains("setScrollerStyle(") {
            // Overlay scrollers: the vendored arithmetic is correct as
            // written and there is nothing to repair.
            return;
        }
        assert!(
            src.contains("fn enforce_scroller_layout("),
            "the scroller style is forced but the layout repair is gone; \
             Scintilla's own tile will let the content view cover the bars"
        );
        // The repair has to re-run after Scintilla re-tiles, which it
        // does on any document resize — so a one-shot call at startup is
        // not enough. `SCN_PAINTED` is the hook that follows those.
        let painted = fn_body(src, "on_sci_notify_inner");
        assert!(
            painted.contains("enforce_scroller_layout("),
            "the layout repair must run from the SCN_PAINTED arm, or it \
             lasts only until the first long line widens the document"
        );
        // Trimming the line-number ruler to the clip's height is only
        // half of keeping it out of the scrollbar band: Scintilla paints
        // the gutter a whole line-row at a time, so the trim leaves a
        // partial row that `PaintMargin` fills in full, and AppKit does
        // not clip a view to its own bounds by default. Without this the
        // gutter draws a few points past its own frame — invisible to
        // every instrument on this backend except a real screenshot,
        // which is why it is guarded here rather than tested.
        // The *call* form, not the bare identifier: the function body
        // carries a comment naming `setClipsToBounds` in prose, and
        // matching that made this guard pass against a mutation that
        // deleted the call. Same trap m3c's dirty-poll guard fell into.
        let repair = fn_body(src, "enforce_scroller_layout");
        assert!(
            repair.contains("ruler.setClipsToBounds("),
            "the ruler is no longer clipped to its bounds, so the gutter's \
             partial last row will overpaint into the scrollbar band"
        );
    }

    /// The horizontal floor must be allowed back down when the viewport
    /// shrinks.
    ///
    /// The floor exists so the blank area right of short lines stays
    /// clickable, and it only ever rises — which is right until the
    /// window gets smaller, at which point the document is still as wide
    /// as the *old* viewport and a horizontal scrollbar appears for
    /// content that is not there. Reported after a maximise and
    /// un-maximise on a three-line file.
    ///
    /// It must come down *only* when the width is exactly the floor this
    /// function last installed. Re-seeding to let tracking recompute is
    /// the tempting alternative and it loses a real long line's scroll
    /// range — see the function.
    #[test]
    fn the_horizontal_floor_is_reseeded_when_the_viewport_changes() {
        let body = fn_body(production_src(), "clamp_scroll_width_to_viewport");
        let width_check = body
            .find("LAST_VIEWPORT_WIDTH")
            .expect("the clamp no longer notices a viewport resize");
        assert!(
            !body.contains("seed_horizontal_scroll("),
            "the clamp must not re-seed: SCI_SETSCROLLWIDTH zeroes \
             lineWidthMaxSeen and tracking only re-raises on a later paint, \
             so reading the width back in the same pass pins it to the \
             viewport and a long line's scroll range is lost"
        );
        let lower = body
            .find("previous > target")
            .expect("the clamp no longer lowers its floor when the viewport shrinks");
        assert!(
            width_check < lower,
            "the lowering must be keyed on the remembered viewport width"
        );
    }

    #[test]
    fn the_tab_drag_loop_stops_touching_self_before_it_mutates() {
        let src = include_str!("tabs.rs");
        let src = match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        let start = src
            .find("drop(freeze);")
            .expect("track() no longer drops its freeze");
        // The tail runs to the end of the function; the next item after
        // it is the `Gesture` enum, which is the natural stop.
        let end = src[start..]
            .find("enum Gesture")
            .expect("Gesture enum no longer follows track()")
            + start;
        let tail = &src[start..end];
        assert!(
            !tail.contains("self."),
            "TabButton::track touches `self` after mutating the model, \
             which resyncs the strip and can deallocate the receiver \
             while its own mouseDown: is still on the stack"
        );
    }

    /// One line of source with its `//` comment and every string literal
    /// removed, so a pattern quoted in a doc comment or in an assertion
    /// message is not counted as a real call.
    ///
    /// `ui_gtk`'s equivalent scanner shipped without this and reported
    /// three `scintilla_new()` calls where there was one.
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

    /// Every source file in this crate, comments and string literals
    /// removed, each cut at its own first test module.
    fn all_production_code() -> String {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = String::new();
        for entry in std::fs::read_dir(&dir)
            .expect("ui_cocoa/src is readable")
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let cut = text.find("#[cfg(test)]").unwrap_or(text.len());
                out.push_str(&code_only(&text[..cut]));
            }
        }
        out
    }

    #[test]
    fn the_scanner_ignores_comments_and_strings() {
        let sample = "\
let a = scintilla_cocoa_new();
// let b = scintilla_cocoa_new();
/// `scintilla_cocoa_new()` returned null
let msg = \"found scintilla_cocoa_new() calls\";
";
        assert_eq!(
            code_only(sample).matches("scintilla_cocoa_new()").count(),
            1
        );
        // And it must not swallow real code that follows a string.
        assert!(
            code_only("let x = \"a\"; scintilla_cocoa_new();").contains("scintilla_cocoa_new()")
        );
    }

    /// Exactly two permanent Scintilla views, never destroyed.
    ///
    /// `EditorHandle` is `Copy`, carries no lifetime, and holds raw
    /// pointers into a view — so nothing in the type system stops a copy
    /// outliving what it points at. This backend discharges that
    /// obligation structurally: the main editor and the Document Map's
    /// miniature are each created once in `run` and never finalised, and
    /// tabs get their own buffers through `SCI_SETDOCPOINTER` rather than
    /// through views of their own.
    ///
    /// DESIGN.md §7.4 names an `NSView`-per-tab design as the specific
    /// mistake this avoids, and says in as many words that a Cocoa
    /// backend which reaches for one view per tab inherits the problem.
    /// A runtime test cannot observe the failure — finalising a view
    /// faults inside vendored C++ on the next direct call rather than
    /// failing an assertion — so the guard is a source scan, the same
    /// tool and the same reasoning as `ui_gtk`'s.
    #[test]
    fn exactly_two_scintilla_views_are_ever_created() {
        let src = all_production_code();
        assert!(
            src.len() > 20_000,
            "scanned only {} bytes; the walk is broken, so a clean result proves nothing",
            src.len()
        );
        let calls = src.matches("scintilla_cocoa_new()").count();
        assert_eq!(
            calls, 2,
            "this backend must build exactly two permanent Scintilla views — the main \
             editor and the Document Map miniature — found {calls}. Each is created once \
             and shares tab documents via SCI_SETDOCPOINTER; a *per-tab* view would leave \
             every copied `EditorHandle` dangling when a tab closes, which is the hazard \
             this count guards. Adding a third permanent view is fine, but update this."
        );
        // The shim exposes no release entry point, so there is no
        // supported way to finalise one — but removing a view from its
        // superview drops the last strong reference and has the same
        // effect. Nothing may do that to either view.
        for forbidden in [
            "sci_view.removeFromSuperview",
            "miniature.removeFromSuperview",
        ] {
            assert!(
                !src.contains(forbidden),
                "`{forbidden}` would drop the last reference to a permanent \
                 Scintilla view and leave its `EditorHandle` dangling"
            );
        }
    }

    /// The Document Map's miniature must stay unfocusable, and the guard
    /// that keeps it so must not depend on a `with_state` borrow.
    ///
    /// Two properties, and both shipped broken once before this test
    /// existed. The miniature is a second Scintilla view bound to the
    /// *same editable document* as the editor, so focus on it means
    /// typing into the user's buffer with no visible caret
    /// (`CARETSTYLE_INVISIBLE`). Measured on the real app: Tab-cycling
    /// reached it and five keystrokes moved the document length.
    ///
    /// The first fix routed the guard through `with_state`, which is
    /// declined re-entrantly — and `makeFirstResponder:` *is* reachable
    /// from inside an outer borrow. A declined guard reads as "not the
    /// map" and **allows** the focus it exists to refuse, so it failed
    /// open and the bug reproduced unchanged. The borrow-free version is
    /// therefore load-bearing, not a tidy-up, and that is what the second
    /// assertion pins.
    ///
    /// A source scan because the failure needs a real window server and a
    /// real key-view loop, and because it fails by *absence* — nothing
    /// errors, focus simply lands somewhere it should not.
    #[test]
    fn the_docmap_miniature_cannot_take_keyboard_focus() {
        let win = include_str!("window.rs");
        let win = match win.find("#[cfg(test)]") {
            Some(i) => &win[..i],
            None => win,
        };
        let body = fn_body(win, "make_first_responder");
        assert!(
            body.contains("docmap::owns_view("),
            "`makeFirstResponder:` no longer consults `docmap::owns_view`, so the \
             Document Map's miniature can take Tab focus — and typing then edits \
             the shared document with no visible caret."
        );

        let map = include_str!("docmap.rs");
        let map = match map.find("#[cfg(test)]") {
            Some(i) => &map[..i],
            None => map,
        };
        assert!(
            !fn_body(map, "owns_view").contains("with_state("),
            "`docmap::owns_view` reads the window state again. That borrow is \
             declined re-entrantly on exactly the path this guard protects, and a \
             declined read reads as `false` — which *allows* the focus. Keep it on \
             the `PANEL_VIEW` thread-local."
        );
    }

    /// The workspace tree must sanitize what it *renders* and must never
    /// launch what it has not re-checked.
    ///
    /// Both halves are the bug DESIGN.md §7.4 records shipping on Win32:
    /// a file named `invoice\u{202E}fdp.exe` rendered as a `.pdf`, and the
    /// context menu's "Run by system" then handed the real `.exe` to
    /// `ShellExecuteW`. The defence is the value-vs-label split — the
    /// label is sanitized once, at insert, and no action ever derives a
    /// path from it — plus a `within_root` re-check at *click* time
    /// rather than at menu-build time.
    ///
    /// A source scan because neither property fails loudly: an
    /// unsanitized label looks like a filename, and a missing re-check
    /// only matters on a root change mid-menu. Both were verified against
    /// the live app as well — the rendered label comes back with U+FFFD
    /// where the override was, while the row's path keeps the real
    /// character.
    #[test]
    fn the_workspace_tree_sanitizes_labels_and_rechecks_before_launching() {
        let src = include_str!("workspace.rs");
        let src = match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };

        // Exactly one place turns a real filename into a label, so there
        // is one place to get it wrong.
        assert_eq!(
            src.matches("sanitize_filename_for_display(").count(),
            1,
            "the workspace tree must sanitize display names in exactly one place — \
             `insert_node`. A second call site is a second thing to forget."
        );
        assert!(
            fn_body(src, "insert_node").contains("sanitize_filename_for_display("),
            "`insert_node` no longer sanitizes the label, so a filename carrying a \
             bidi override can spoof its extension in the panel"
        );

        // Every action that opens or launches re-checks at click time.
        //
        // Matched on the **early-return gate** rather than on the bare
        // identifier, and that is not pedantry: the first version of this
        // assertion looked for `within_root(` anywhere in the body, and a
        // mutation that deleted the gate outright still passed, because
        // one *arm* of the same function happens to call it too. A guard
        // satisfied by a different call than the one it is guarding is
        // not a guard.
        for (function, gate) in [
            ("context_command", "if !within_root(&path) {"),
            ("activate_selected_row", "if !within_root(&path) {"),
        ] {
            assert!(
                fn_body(src, function).contains(gate),
                "`{function}` no longer opens with `{gate}`, so a root change \
                 between building the action and invoking it could reach a path \
                 outside the workspace"
            );
        }

        // And the one function that hands a path to the system is only
        // ever reached from a checked caller.
        assert!(
            fn_body(src, "open_in_default_app").contains("NSWorkspace"),
            "`open_in_default_app` no longer looks like the single launch site; \
             if launching moved, the `within_root` guards above may no longer \
             cover it"
        );
    }

    /// Every panel `build_content` builds must be parented to the content
    /// view.
    ///
    /// This shipped broken: `workspace.container` was constructed, stored
    /// on the state, laid out by `relayout_chrome` — and never added as a
    /// subview. The panel was a blank rectangle, reported by a user.
    ///
    /// **Nothing else could have caught it, which is the point.** The
    /// build succeeds; clippy sees the panel used; a diff cannot show a
    /// line that was never written, so both reviewers passed over it. And
    /// a detached `NSView` answers every question a probe thinks to ask —
    /// it keeps its frame, reports `isHidden == false`, hands out its
    /// subviews, and even lets AppKit make cell views for it. My own
    /// verification checked all of those and cleared it. The one question
    /// that distinguishes attached from detached is `superview()`, and
    /// nothing asked it.
    ///
    /// The list is derived from the function's **return tuple** rather
    /// than hard-coded, so a future panel is covered the moment it is
    /// returned — a hard-coded list would need updating by exactly the
    /// person who just forgot the `addSubview`.
    #[test]
    fn every_panel_is_added_to_the_content_view() {
        let body = fn_body(production_src(), "build_content");
        // The last tuple expression in the body is the return.
        let start = body.rfind("\n    (").expect("no return tuple") + 6;
        let end = body[start..].find(')').expect("unterminated tuple") + start;
        let returned: Vec<&str> = body[start..end]
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        assert!(
            returned.len() >= 5,
            "parsed only {returned:?} from the return tuple; the scan is broken, \
             so a clean result proves nothing"
        );
        for member in returned {
            let call = format!("content.addSubview(&{member}.container)");
            assert!(
                body.contains(&call),
                "`build_content` returns `{member}` but never calls `{call}` — so \
                 that panel is built, stored and laid out, and never drawn. A \
                 detached view still reports its frame, its subviews and \
                 `isHidden == false`, so nothing at runtime notices."
            );
        }
        // The editor is not a panel and is added by its own name.
        assert!(body.contains("content.addSubview(sci_view)"));
    }

    /// Unfold All's ceilings must not be defeated by the final reveal.
    ///
    /// `expandItem:expandChildren:YES` walks every reachable row and
    /// fires `outlineViewItemWillExpand:` for each — which is the lazy
    /// *loader*. So using it to reveal the result of a walk that stopped
    /// at a ceiling re-reads every folder the ceiling declined,
    /// recursively and synchronously, with no bound at all. Measured on a
    /// 340-directory tree with the folder ceiling lowered to 5: the
    /// correct reveal populates 20 directories, the recursive one
    /// populates all 341.
    ///
    /// A source scan because the failure is a *hang* on a pathological
    /// tree, which is exactly what no test suite wants to reproduce, and
    /// because the wrong call is the one that reads as obviously correct.
    #[test]
    fn the_unfold_reveal_cannot_re_enter_the_lazy_loader() {
        let src = include_str!("workspace.rs");
        let src = match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(
            !src.contains("expandItem_expandChildren("),
            "the workspace tree uses `expandItem:expandChildren:`, which fires \
             `outlineViewItemWillExpand:` for every row it reaches and so re-enters \
             `populate_if_needed` — defeating UNFOLD_MAX_FOLDERS/UNFOLD_MAX_ROWS \
             entirely. Reveal with `expand_populated`, which expands only folders \
             already read."
        );
        assert!(
            fn_body(src, "tick_unfold").contains("begin_reveal("),
            "the walk no longer hands off to the batched reveal phase"
        );

        // And the reveal itself must stay batched. Expanding 3 682
        // folders in one pass froze the app for 18 seconds — reported by
        // a user, and a freeze here blocks the editor and every other
        // tab, not just this panel. Both halves matter: the coalescing
        // (185× on its own) and the yielding.
        let reveal = fn_body(src, "tick_reveal");
        assert!(
            reveal.contains("beginUpdates()") && reveal.contains("endUpdates()"),
            "the reveal no longer coalesces its expansions — each `expandItem:` \
             recomputes the view's row array, so without this the cost is \
             quadratic in the tree (measured: 18s versus 97ms on 3 682 folders)"
        );
        assert!(
            reveal.contains("UNFOLD_TICK_BUDGET_MS"),
            "the reveal no longer yields on a time budget, so one tick can grow \
             without bound on a large tree"
        );
        assert!(
            reveal.contains("schedule_reveal_tick("),
            "the reveal no longer reschedules itself, so it would stop partway"
        );
    }

    /// The map's column must be recomputed on every layout, and a closed
    /// panel must never be resized to zero.
    ///
    /// Both halves are regressions waiting to happen. Dropping the
    /// `width_for_layout` call would leave the editor full-width with the
    /// map painted over it; dropping the `map_w > 0.0` guard reintroduces
    /// the bug this milestone hit — a hidden panel's width-sizable
    /// subviews collapse with it, and autoresizing cannot restore
    /// proportions from a zero-width superview, so reopening came back
    /// with the header label stretched over the close button. The FIF
    /// dock carries the same pair of guards for the same reason.
    #[test]
    fn the_layout_clamps_the_map_and_never_collapses_it() {
        let src = include_str!("platform.rs");
        let src = match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        let body = fn_body(src, "relayout_chrome");
        assert!(
            body.contains("docmap::width_for_layout(")
                && body.contains("workspace::width_for_layout("),
            "`relayout_chrome` no longer clamps the Document Map's width against \
             the live window, so a persisted width can starve the editor"
        );
        assert!(
            body.contains("if map_w > 0.0") && body.contains("if ws_w > 0.0"),
            "`relayout_chrome` no longer guards the map's frame update. Resizing a \
             hidden panel to zero collapses its width-sizable subviews, and they do \
             not come back proportionally when it reopens."
        );
    }

    #[test]
    fn the_delegate_calls_activate_main_window() {
        let src = include_str!("delegate.rs");
        assert!(src.len() > 1_000, "source scan read too little to be real");
        assert!(
            src.contains("applicationDidFinishLaunching:"),
            "the delegate no longer implements applicationDidFinishLaunching:"
        );
        assert!(
            src.contains("activate_main_window"),
            "applicationDidFinishLaunching: no longer activates the window"
        );
    }

    /// The Language menu's group marks must match on the **action** as
    /// well as the tag.
    ///
    /// A menu item's tag defaults to 0, which is `L_TEXT`'s own language
    /// id — so a submenu whose children are not language rows is marked
    /// for every buffer whose language is Normal Text, i.e. every
    /// untitled one. That is not hypothetical: adding the
    /// "User-Defined language" submenu, whose three items carry no tag,
    /// produced exactly this, and it was caught by driving the real app
    /// (`active_lang=Some(0)`, submenu ticked) rather than by the suite.
    ///
    /// A source scan because painting a mark needs a real `NSMenu` and
    /// a delegate callback AppKit makes only at display time.
    #[test]
    fn the_language_marks_match_the_action_not_only_the_tag() {
        let src = include_str!("menu.rs");
        let src = match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(src.len() > 5_000, "source scan read too little to be real");
        let body = fn_body(src, "mark_language_letter_groups");
        assert!(
            body.contains("child.action() == Some(sel!(codeppSetLanguage:))"),
            "`mark_language_letter_groups` matches on the tag alone again. Tag 0 is \
             `L_TEXT`, so any submenu holding a tagless item — the \"User-Defined \
             language\" submenu does — will tick for every untitled buffer."
        );
    }

    /// The loaded-UDL rows must be rebuilt *before* the marks are set.
    ///
    /// Both run from one `menuNeedsUpdate:`, and the order decides
    /// whether a UDL row added on this pass can carry the mark on the
    /// same pass. Reversed, the first open after a UDL is installed
    /// shows the row unmarked even when it is the active language.
    #[test]
    fn the_udl_rows_are_rebuilt_before_the_marks_are_set() {
        let src = include_str!("menu.rs");
        let src = match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        let body = fn_body(src, "menu_needs_update");
        let rebuild = body
            .find("rebuild_udl_rows(")
            .expect("`menuNeedsUpdate:` no longer rebuilds the loaded-UDL rows");
        let mark = body
            .find("mark_language_letter_groups(")
            .expect("`menuNeedsUpdate:` no longer refreshes the Language menu marks");
        assert!(
            rebuild < mark,
            "the Language menu marks are set before the UDL rows exist, so a row \
             added on this pass cannot be marked on it"
        );
    }

    /// `SCN_STYLENEEDED` must reach the UDL tokeniser.
    ///
    /// A UDL runs in `SCLEX_CONTAINER`, where Scintilla does no
    /// tokenising of its own — it asks the host. A backend that drops
    /// the notification renders every UDL buffer entirely unstyled, and
    /// nothing else misbehaves, so the symptom reads as "the UDL is
    /// broken" rather than "the host never answered".
    #[test]
    fn style_needed_drives_the_udl_tokeniser() {
        let src = production_src();
        let body = fn_body(src, "on_sci_notify_inner");
        assert!(
            body.contains("SCN_STYLENEEDED") && body.contains("udl::on_style_needed("),
            "`on_sci_notify_inner` no longer routes SCN_STYLENEEDED to the UDL \
             tokeniser, so UDL buffers render unstyled"
        );
    }

    /// The UDL paint must happen **after** the state borrow is released.
    ///
    /// Every `SCI_SETSTYLING` re-enters this backend's notification
    /// dispatch, and `with_state` declines a nested borrow rather than
    /// panicking — so painting inside the closure would silently skip
    /// each refresh those notifications drive. This backend has been
    /// caught by that contract three times already (m4a's Replace, the
    /// save-then-close gate, and the save-point marker), which is why it
    /// is pinned here rather than left to the comment.
    #[test]
    fn the_udl_paint_runs_outside_the_state_borrow() {
        let src = include_str!("udl.rs");
        let body = fn_body(src, "on_style_needed");
        let closure_end = body
            .find("});")
            .expect("`on_style_needed` no longer captures under a with_state closure");
        let paint = body
            .find("paint_style_needed(")
            .expect("`on_style_needed` no longer paints");
        assert!(
            paint > closure_end,
            "`on_style_needed` paints inside the `with_state` closure. Each \
             SCI_SETSTYLING re-enters the notification dispatch, where the held \
             borrow makes every refresh a silent no-op."
        );
    }

    /// ⇧⌘T must live on a **static** File-menu item.
    ///
    /// AppKit does not call `menuNeedsUpdate:` when it searches for a key
    /// equivalent, so a shortcut on a delegate-built item is dead until
    /// the user happens to open that menu once. Measured with a control:
    /// populated, the same synthetic ⇧⌘T resolves and opens a file;
    /// unpopulated it resolves to nothing on `NSMenu`'s own
    /// `performKeyEquivalent:`, on `NSApp::sendEvent`, and when posted.
    ///
    /// A source scan because the failure is *absence* — the command
    /// simply never runs — and observing it needs a real menu, a real
    /// key event and a populated recent-files list.
    #[test]
    fn the_restore_shortcut_is_not_on_a_delegate_built_item() {
        let src = include_str!("menu.rs");
        let src = match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(src.len() > 5_000, "source scan read too little to be real");
        assert!(
            fn_body(src, "add_recent_files_tail").contains("codeppRestoreRecentClosed:"),
            "Restore Recent Closed File left the File menu's static tail. Its ⇧⌘T \
             then only works after the menu has been opened once."
        );
        assert!(
            !fn_body(src, "rebuild_recent_region").contains("codeppRestoreRecentClosed:"),
            "Restore Recent Closed File is built by the menu delegate again, so its \
             ⇧⌘T is dead until the File menu is first opened."
        );
    }

    /// The recent-files rows must be removed *before* the state is read.
    ///
    /// Their tag is a list **index**, and `open_recent_at` resolves it
    /// against the live list at click time — so a row left in place when
    /// the read is declined could open a file other than the one it
    /// names. Ordering is the whole fix, and it is invisible at the call
    /// site: both orders compile, and both look right until the list
    /// changes under a declined rebuild. The Window and Language menus
    /// deliberately do the opposite, because their tags (a `Tab.id`, a
    /// language id) stay meaningful when stale.
    #[test]
    fn stale_recent_rows_are_cleared_before_the_state_is_read() {
        let src = include_str!("menu.rs");
        let src = match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        let body = fn_body(src, "rebuild_recent_region");
        let remove = body
            .find("menu.removeItemAtIndex(anchor)")
            .expect("`rebuild_recent_region` no longer removes the previous rows");
        let read = body
            .find("crate::recent_file_rows()")
            .expect("`rebuild_recent_region` no longer reads the recent-files list");
        assert!(
            remove < read,
            "`rebuild_recent_region` reads the state before clearing the old rows, so \
             a declined read leaves rows whose index tag can open the wrong file"
        );
    }

    /// A recent-files label is a path, and a path is attacker-influenced
    /// display text. Same value-vs-label split as the workspace tree and
    /// the Find-in-Files dock: the row's *functional* value is its index,
    /// carried in the tag, and is never parsed back out of the label.
    #[test]
    fn recent_file_labels_are_sanitized() {
        let src = production_src();
        assert!(
            fn_body(src, "recent_file_rows").contains("sanitize_str_for_display("),
            "recent-files menu labels are no longer sanitized, so a filename \
             carrying bidi controls renders reordered in the File menu"
        );
    }
}
