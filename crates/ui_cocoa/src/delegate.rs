//! The `NSApplicationDelegate`.
//!
//! m1 shipped without one and recorded three consequences in DESIGN.md
//! §7.4; all three are the same hook, which is why they land together:
//!
//! 1. **Closing the window left a menu-bar-only process.** AppKit's
//!    default is to keep running with no windows, which is right for a
//!    Mail or Photos, and wrong for a single-window editor (DESIGN.md
//!    §10 fixes Code++ as single-window, so the answer to
//!    `applicationShouldTerminateAfterLastWindowClosed:` is
//!    unconditionally yes).
//! 2. **Nothing after `app.run()` was reachable.**
//!    `-[NSApplication terminate:]` calls `exit()`, so `run()` never
//!    returns and the `perf.report()` / `state::uninstall()` that
//!    `ui_gtk` does after its main loop simply never happened.
//!    `applicationWillTerminate:` is the correct home for both.
//! 3. **The session had no save-on-quit.** Same hook again.
//!
//! Ordering inside `applicationWillTerminate:` matters and is asserted
//! by the code rather than left to chance: run `crate::quit` *first* —
//! it tells the plugins they are shutting down and saves the session,
//! which reads the caret back out of the live editor, so it needs the
//! state intact — then report perf, then tear the state down.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSApplicationDelegate};
use objc2_foundation::{MainThreadMarker, NSNotification, NSObject, NSObjectProtocol};

define_class!(
    // SAFETY: `NSObject` is the correct superclass for a delegate that
    // implements no AppKit class's behaviour, only its protocol. The
    // class is main-thread-only because every callback AppKit makes on
    // it arrives on the main thread and the handlers touch the
    // main-thread `thread_local` state.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppApplicationDelegate"]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        /// Quit when the last (and only) window closes.
        ///
        /// Without this the red traffic-light button leaves a running
        /// process with a menu bar and no way back to a window — m1's
        /// most visible rough edge.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            true
        }

        /// Order the window front and activate, once the run loop is
        /// actually turning.
        ///
        /// **This is why the app responds to the mouse at all.** Doing
        /// it before `-[NSApplication run]` — which is where `run`
        /// naturally builds the window — silently does nothing for a
        /// process that is not inside an `.app` bundle: the window never
        /// becomes key and the application never becomes active, so the
        /// window server does not route clicks to it. Keyboard input can
        /// still appear to work, which makes the failure look like
        /// "mouse is broken" rather than "the app was never activated".
        ///
        /// Measured, not inferred. With activation left before `run()`,
        /// a bare `./codepp` reports
        /// `app_active=false has_key_window=false` from inside the run
        /// loop, while the *same binary* copied into a minimal `.app`
        /// and launched with `open` reports all true. Moving activation
        /// here makes the bare binary report all true as well — which
        /// matters because `cargo run` is the documented development
        /// workflow (DEVELOPMENT.md §4.5).
        ///
        /// Then the plugins whose dock panels the restored session had
        /// open are loaded and those panels brought back, by running
        /// their own commands the way Notepad++ restores a plugin panel
        /// — after the window is on screen, as Win32 and GTK do, and
        /// before the first frame paints, so that frame already carries
        /// the panels. Each step at its own boundary: a failed restore
        /// must not cost the window its focus, nor the reverse.
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            crate::at_callback_boundary("applicationDidFinishLaunching:", (), || {
                crate::activate_main_window();
            });
            crate::at_callback_boundary(
                "applicationDidFinishLaunching:restore",
                (),
                crate::plugin::restore_panel_plugins,
            );
            // Last: a plugin's command may have focused its own panel.
            crate::at_callback_boundary(
                "applicationDidFinishLaunching:focus",
                (),
                crate::focus_editor,
            );
        }

        /// Re-order the floating dock panels front once the application
        /// is active.
        ///
        /// They are `hidesOnDeactivate` panels, and on a cold start with
        /// a floating group in `session.xml` they are ordered front
        /// *before* the application has ever become active — the same
        /// moment the main window's own `orderFront` used to be lost
        /// (see `did_finish_launching` above). AppKit restores hidden
        /// panels on the activate transition when it hid them itself;
        /// this covers the case where it never had them to hide.
        /// Idempotent for a float that is already on screen.
        #[unsafe(method(applicationDidBecomeActive:))]
        fn did_become_active(&self, _notification: &NSNotification) {
            crate::at_callback_boundary(
                "applicationDidBecomeActive:",
                (),
                crate::dock::order_floats_front,
            );
        }

        /// Persist and tear down before `exit()`.
        ///
        /// This is the *only* reliable shutdown hook on macOS: the Quit
        /// menu item and ⌘Q both route through `terminate:`, which calls
        /// `exit()` and never unwinds the Rust stack.
        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            // Each step is guarded separately rather than the three
            // together: this is the last code to run before `exit()`, so
            // a panic in the session save must not cost the teardown, and
            // a panic in either must not cost the other.
            crate::at_callback_boundary("applicationWillTerminate:quit", (), || {
                // Order is load-bearing. The quit tells the plugins and
                // saves the session, which reads the caret position back
                // out of the live Scintilla view, so it has to run while
                // the state is still installed. A no-op when the main
                // window's close button already ran it.
                crate::quit();
            });
            crate::at_callback_boundary("applicationWillTerminate:perf", (), crate::report_perf);
            // Drop the state so `Shell` — and the worker threads its
            // channels keep alive — tear down deterministically rather
            // than being abandoned by `exit()`.
            crate::at_callback_boundary("applicationWillTerminate:teardown", (), || {
                crate::state::uninstall();
            });
        }
    }
);

impl AppDelegate {
    /// Construct and install the delegate on `app`.
    ///
    /// The returned value must be kept alive for the life of the
    /// process: `NSApplication.delegate` is a **weak** reference (an
    /// unowned one, in AppKit's terms), so dropping this would leave
    /// AppKit messaging a freed object at quit time — precisely when the
    /// handlers above matter most.
    pub fn install(app: &NSApplication, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `init` on a freshly allocated instance of our own
        // class, which adds no ivars needing other initialisation.
        let delegate: Retained<Self> = unsafe { msg_send![this, init] };
        let proto = ProtocolObject::from_ref(&*delegate);
        app.setDelegate(Some(proto));
        delegate
    }
}
