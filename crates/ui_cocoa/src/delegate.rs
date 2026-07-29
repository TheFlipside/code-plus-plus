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
//! by the code rather than left to chance: save the session *first*
//! (it reads the caret back out of the live editor, so it needs the
//! state intact), then report perf, then tear the state down.

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

        /// Persist and tear down before `exit()`.
        ///
        /// This is the *only* reliable shutdown hook on macOS: the Quit
        /// menu item and ⌘Q both route through `terminate:`, which calls
        /// `exit()` and never unwinds the Rust stack.
        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            // Order is load-bearing. `save_session_now` reads the caret
            // position back out of the live Scintilla view, so it has to
            // run while the state is still installed.
            crate::save_session_now();
            crate::report_perf();
            // Drop the state so `Shell` — and the worker threads its
            // channels keep alive — tear down deterministically rather
            // than being abandoned by `exit()`.
            crate::state::uninstall();
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
