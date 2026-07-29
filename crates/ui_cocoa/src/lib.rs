//! Cocoa UI backend for Code++.
//!
//! Scope so far (Phase 5 m1): macOS opens a window hosting a real
//! Scintilla view with the DESIGN.md §4.2 direct-call pair captured, an
//! application menu, and an Edit menu wired to the standard AppKit
//! action selectors Scintilla already implements. `Shell` wiring, the
//! status bar, tabs, the toolbar, dialogs and the plugin host are later
//! milestones. (Plain code spans, not intra-doc links, for cross-crate
//! references in this file — `ui_cocoa` deliberately does not depend on
//! `ui_win32` or `ui_gtk`, so links to them would be unresolvable and
//! would warn on `cargo doc`.)
//!
//! # Why the menus exist this early
//!
//! They are not decoration and not an m3 preview. AppKit routes ⌘-chords
//! through menu-item key equivalents, so without a menu there is no path
//! from a keypress to `-[SCIContentView undo:]` / `selectAll:` — the
//! methods Scintilla already implements (`cocoa/ScintillaView.mm:913`,
//! `:943`). Typing works without a menu because that arrives through
//! `keyDown:`; ⌘Z and ⌘A do not. m1's acceptance criteria include both,
//! so the minimum menu that makes them reachable is m1 scope.
//!
//! # Why no `NSDocument` / `NSApplicationDelegate`
//!
//! Same reasoning as `ui_gtk` declining `gtk::Application` and
//! `ui_win32` calling `CreateWindowExW` directly: Code++'s cold-start
//! budget is 80 ms (DESIGN.md §8) and the document architecture's
//! machinery is not on the critical path to the first frame. This
//! backend drives `NSApplication` directly.

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

use std::fmt;
use std::path::PathBuf;

use codepp_core::perf::Perf;
use codepp_editor::EditorHandle;
use codepp_scintilla_sys::scintilla_cocoa_new;

use objc2::rc::Retained;
use objc2::sel;
// `alloc` on a main-thread-only AppKit class comes from this trait, not
// from the class itself — it is the associated function that consumes
// the `MainThreadMarker`.
use objc2::MainThreadOnly;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSMenu, NSMenuItem, NSView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

/// Initial window size, matching the other two backends' defaults.
const DEFAULT_WIDTH: f64 = 1024.0;
const DEFAULT_HEIGHT: f64 = 768.0;

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
        }
    }
}

impl std::error::Error for CocoaUiError {}

/// Build the window, host a Scintilla view, and run the AppKit event
/// loop until the user quits.
///
/// `perf` carries the clock `main` started; it is inert unless `--perf`
/// was passed. See `codepp_core::perf` for what is measured and why the
/// clock is not started here.
///
/// `initial_path` is accepted and currently unused — file loading
/// arrives with the `Shell` wiring in m2. It is in the signature now so
/// `codepp-app`'s three backend arms stay identical in shape.
///
/// # Errors
///
/// Returns [`CocoaUiError`] if not called on the main thread, if
/// Scintilla will not construct its view, or if the direct-call pair
/// cannot be captured. All three are fatal setup failures.
// Both parameters are taken by value because this signature is a
// cross-backend contract: `codepp-app` has one arm per backend and they
// must stay identical in shape, and the sibling `run`s consume both
// (`ui_gtk` moves `perf` into an `Rc` and hands `initial_path` to
// session restore). m1 only reads them because the `Shell` wiring that
// consumes them is m2. Narrowing to references here would mean widening
// them back one milestone later, and would make this backend's
// signature the odd one out in the meantime.
#[allow(clippy::needless_pass_by_value)]
pub fn run(initial_path: Option<PathBuf>, perf: Perf) -> Result<(), CocoaUiError> {
    if let Some(path) = initial_path.as_ref() {
        // m2 wires `Shell` and with it file loading. Logged rather than
        // silently dropped so a user passing a path on the command line
        // in the meantime gets an explanation from `--verbose`.
        // Debug sigil, not Display: a path is attacker-influenced input
        // and `Display` would pass control characters straight into the
        // log. Enforced workspace-wide by
        // `codepp_core::tracing_sigil_guard`.
        tracing::info!(
            ?path,
            "ui_cocoa m1 does not load files yet; opening an empty buffer"
        );
    }

    // AppKit is main-thread-only, and `MainThreadMarker` is objc2's way
    // of proving that once rather than re-asserting it at every call.
    // Every AppKit constructor below takes it, so the check cannot be
    // skipped by accident.
    let mtm = MainThreadMarker::new().ok_or(CocoaUiError::NotMainThread)?;

    let app = NSApplication::sharedApplication(mtm);
    // `Regular` gives a Dock icon, a menu bar, and — the part that
    // matters — the ability to become the active application and
    // receive key events. A plain `cargo run` binary is not in an
    // `.app` bundle, so without this the process launches as an
    // accessory and the window never takes focus.
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    install_menu_bar(&app, mtm);

    // --- The Scintilla view ---------------------------------------
    //
    // Created exactly once, and never destroyed, removed or reassigned
    // for the life of the process. Tabs will switch buffers underneath
    // it with `SCI_SETDOCPOINTER` (m2/m3) rather than getting their own
    // views. That is what makes `EditorHandle`'s raw pointers sound —
    // see `EditorHandle::from_cocoa_view`'s safety documentation, and
    // DESIGN.md §7.4, which names an `NSView`-per-tab design as the
    // specific mistake this avoids.
    //
    // SAFETY: `NSApplication::sharedApplication` has run, which is
    // `scintilla_cocoa_new`'s only precondition beyond being on the
    // main thread — proven above by `mtm`.
    let sci_ptr = unsafe { scintilla_cocoa_new() };
    if sci_ptr.is_null() {
        return Err(CocoaUiError::ScintillaCreate);
    }

    // Adopt the +1 reference the shim handed out, **before** anything
    // that can fail. `from_raw` takes ownership of exactly that
    // reference rather than adding another, which is why the shim
    // returns through `__bridge_retained` — see its ARC section.
    //
    // The ordering matters: adopting first means every early return
    // below drops `sci_view` and releases the view through the normal
    // `Drop`. Capturing the direct-call pair first (the obvious reading,
    // since it wants a raw pointer) would leak the view on the
    // `DirectCallCapture` path, because `sci_ptr` is a bare `*mut
    // c_void` that nothing releases. That path is a fatal setup error
    // that exits the process moments later, so the leak would be
    // harmless in practice — but "harmless in practice" is not a
    // property to design an ownership boundary around, and this costs
    // nothing.
    //
    // SAFETY: `sci_ptr` is a non-null, +1-retained `ScintillaView`,
    // which is an `NSView` subclass (`cocoa/ScintillaView.h:79`).
    let sci_view: Retained<NSView> = unsafe { Retained::from_raw(sci_ptr.cast::<NSView>()) }
        .ok_or({
            // Unreachable — the null case was rejected above. Mapped
            // rather than unwrapped so a future edit that moves the
            // null check cannot turn this into a panic.
            CocoaUiError::ScintillaCreate
        })?;

    // Capture the direct-call pair once, here, per DESIGN.md §4.2 —
    // every hot-path operation from now on bypasses the Objective-C
    // message send entirely.
    //
    // Reads through `sci_ptr` while `sci_view` owns the reference, which
    // is fine: `from_cocoa_view` only sends two query messages and takes
    // no ownership.
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
    // backing window server resources now rather than on first display.
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
    // `window` is a `Retained<NSWindow>` that does, and is the only one.
    // Leaving the default would let closing the window deallocate it
    // while a live smart pointer still believed it owned a +1, so the
    // eventual `Drop` would release freed memory.
    //
    // It does not crash today only because `terminate:` calls `exit()`
    // and `Drop` is never reached (see the note above `perf.report()`),
    // which is incidental AppKit behaviour rather than anything this
    // code arranges. Fixed here rather than relied upon, and before the
    // same "own the `Retained<NSWindow>`" pattern is copied into the m3
    // dialogs, which *will* be opened and closed as ordinary control
    // flow.
    //
    // SAFETY: objc2 marks this `unsafe` because setting it to `true` on
    // a window the caller keeps a `Retained` to is what creates the
    // unbalanced-release hazard. Passing `false` is the safe direction:
    // it removes the extra release rather than arming it.
    unsafe { window.setReleasedWhenClosed(false) };

    window.setTitle(&NSString::from_str("Code++"));
    window.setContentView(Some(&sci_view));
    window.center();

    // Focus the editor so the first keystroke lands in the buffer.
    window.makeFirstResponder(Some(&sci_view));
    window.makeKeyAndOrderFront(None);

    app.activate();

    // The window is on screen and the event loop is about to take over,
    // which is the closest honest analogue of the other backends' first
    // draw. Recorded before `run()` because `run()` does not return
    // until quit.
    perf.mark_first_draw();

    // Blocks until the user quits. `editor` is captured here purely to
    // pin the direct-call handle's lifetime to the loop for m1; m2 hands
    // it to `Shell` and the binding stops being inert.
    let _ = &editor;
    app.run();

    // **This is very likely unreachable on the standard Quit path**, and
    // that is a known m1 gap rather than an oversight.
    // `-[NSApplication terminate:]` — what ⌘Q and the Quit menu item
    // send — calls `exit()` directly once the delegate agrees, so
    // `run()` does not return and nothing after it executes. It costs
    // nothing in m1 because there is no keystroke instrumentation on
    // this backend yet, so the report would be empty either way; the
    // cold-start figure is emitted by `mark_first_draw` above and does
    // print.
    //
    // m2 has to fix this properly, and already needs the machinery: it
    // installs an `NSApplicationDelegate` for session save-on-quit, and
    // `applicationWillTerminate:` is the correct home for this call.
    // The same delegate answers
    // `applicationShouldTerminateAfterLastWindowClosed:`, which is what
    // stops closing the window leaving a menu-bar-only process behind —
    // m1's other known rough edge.
    perf.report();
    Ok(())
}

/// Install the application and Edit menus.
///
/// Two menus in m1, both minimal and both load-bearing rather than
/// cosmetic — see the module docs for why ⌘-chords need menu items at
/// all. The full Notepad++ menu tree lands in m3.
///
/// Every item here targets a **standard AppKit action selector with a
/// nil target**, so AppKit sends it down the responder chain and it
/// lands on whichever view is first responder. Scintilla's content view
/// implements all of them (`cocoa/ScintillaView.mm:913` `selectAll:`,
/// `:928` `copy:`, `:943` `undo:`, and the rest alongside) plus
/// `validateUserInterfaceItem:` (`:966`), so the items also grey
/// themselves out correctly with no work on our side. That is why m1
/// gets working undo/redo/clipboard without a single command handler.
fn install_menu_bar(app: &NSApplication, mtm: MainThreadMarker) {
    let main_menu = NSMenu::new(mtm);

    // --- Application menu -----------------------------------------
    //
    // macOS mandates this menu and gives it the process name; it is
    // where About and Quit belong on this platform, which is why they
    // are not under `?` and `File` as they are on Win32/GTK. Recorded
    // as a deliberate divergence in the Phase 5 plan rather than an
    // oversight.
    //
    // The first item of the main menu is *always* treated as the
    // application menu regardless of its title, so the title here is
    // never displayed.
    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);

    add_item(
        &app_menu,
        mtm,
        "About Code++",
        sel!(orderFrontStandardAboutPanel:),
        "",
    );
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(&app_menu, mtm, "Hide Code++", sel!(hide:), "h");
    add_item(
        &app_menu,
        mtm,
        "Hide Others",
        sel!(hideOtherApplications:),
        "",
    );
    add_item(&app_menu, mtm, "Show All", sel!(unhideAllApplications:), "");
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(&app_menu, mtm, "Quit Code++", sel!(terminate:), "q");

    app_item.setSubmenu(Some(&app_menu));
    main_menu.addItem(&app_item);

    // --- Edit menu -------------------------------------------------
    //
    // The subset of Notepad++'s Edit menu whose actions Scintilla
    // already answers. The rest of the tree (File, Search, View, …)
    // arrives in m3 with the command routing it needs.
    let edit_item = NSMenuItem::new(mtm);
    let edit_menu = NSMenu::new(mtm);
    edit_menu.setTitle(&NSString::from_str("Edit"));

    add_item(&edit_menu, mtm, "Undo", sel!(undo:), "z");
    // ⇧⌘Z. `setKeyEquivalentModifierMask` would be the explicit
    // spelling, but AppKit's convention — and every native editor's
    // behaviour — is that an uppercase key equivalent implies Shift.
    add_item(&edit_menu, mtm, "Redo", sel!(redo:), "Z");
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(&edit_menu, mtm, "Cut", sel!(cut:), "x");
    add_item(&edit_menu, mtm, "Copy", sel!(copy:), "c");
    add_item(&edit_menu, mtm, "Paste", sel!(paste:), "v");
    add_item(&edit_menu, mtm, "Delete", sel!(delete:), "");
    add_item(&edit_menu, mtm, "Select All", sel!(selectAll:), "a");

    edit_item.setSubmenu(Some(&edit_menu));
    main_menu.addItem(&edit_item);

    app.setMainMenu(Some(&main_menu));
}

/// Append one nil-targeted menu item.
///
/// A nil target is what makes responder-chain dispatch happen: AppKit
/// walks from the first responder outwards looking for something that
/// implements `action`. Passing an explicit target would bypass the
/// chain and the item would never reach the Scintilla view.
///
/// `key` is the key equivalent without the ⌘; an empty string means no
/// shortcut. An uppercase letter implies Shift by AppKit convention.
fn add_item(
    menu: &NSMenu,
    mtm: MainThreadMarker,
    title: &str,
    action: objc2::runtime::Sel,
    key: &str,
) {
    // SAFETY: standard `NSMenuItem` designated initialiser. The
    // selector is a compile-time `sel!` literal, and a nil target is
    // explicitly supported — it is what requests responder-chain
    // dispatch.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    menu.addItem(&item);
}
