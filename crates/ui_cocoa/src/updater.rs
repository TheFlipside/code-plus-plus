//! The auto-updater on macOS: Sparkle, loaded when a check needs it
//! (DESIGN.md §11).
//!
//! Sparkle checks the feed, shows its windows, verifies the download
//! against the key in `Info.plist`, replaces the bundle and relaunches.
//! Code++ decides *when* (§11.3), and keeps the library out of every
//! session that needs no check:
//!
//! * Nothing here runs before the first paint (§11.2). The paint
//!   notification calls [`after_paint`], which only queues [`start`].
//! * [`start`] establishes whether this copy can update itself: the
//!   bundle's `Info.plist` names a feed and a key, which the release
//!   packaging writes (`.forgejo/scripts/package-unix.sh`), and
//!   `Sparkle.framework` is in the bundle. A build tree has neither, and
//!   so has no updater; "?" → "Update Code++" and the Auto-updater
//!   preference stay greyed there (§11.5).
//! * [`tick`], at the start and every hour, asks on a thread of its own
//!   whether a check is due (`codepp_core::update::check_is_due`, from
//!   `updater.xml`). Only then is the framework loaded, on that thread,
//!   and the check asked for on the main thread, where Sparkle's objects
//!   live. Its own scheduler is off (`SUEnableAutomaticChecks` is false
//!   in `Info.plist`), so this is the only schedule there is.
//! * [`check_now`] is the menu entry: the same load, then a check with
//!   Sparkle's window, whatever the preference says.
//!
//! The framework is not linked: `codepp` names no Sparkle symbol, and
//! every call goes through the Objective-C runtime by name once
//! `dlopen` has registered the classes. It is never unloaded.
//!
//! An update Sparkle installs ends in `terminate:`, the one quit path
//! this backend has, so the session is saved as at any quit.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::OnceLock;

use block2::RcBlock;
use codepp_core::update;
use codepp_platform::dynlib::DynLib;
use codepp_shell::updater_state::{now, StateStore};
use dispatch2::DispatchQueue;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, NSObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{
    ns_string, NSArray, NSBundle, NSDefaultRunLoopMode, NSObjectProtocol, NSRunLoop, NSString,
    NSTimer,
};

use crate::state::with_state;

/// How often [`tick`] asks again whether a check is due, for a session
/// that stays open past the interval.
const RECHECK_EVERY_SECS: f64 = 60.0 * 60.0;

/// The framework's binary, below the bundle's `Frameworks` folder.
const FRAMEWORK_BINARY: &str = "Sparkle.framework/Sparkle";

/// The title of the dialog that says a check could not be made.
const DIALOG_TITLE: &str = "Update Code++";

/// Where the start of the updater stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    /// No frame has been drawn yet.
    BeforeFirstPaint,
    /// A frame is drawn and [`start`] is queued or has run.
    Asked,
}

/// Which check to ask Sparkle for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    /// The scheduled one: a window only if there is an update.
    Background,
    /// The user's: Sparkle's progress window, and its answer either way.
    WithWindow,
}

/// Sparkle's controller and the delegate it reports to. Sparkle holds
/// its delegate weakly, so both are kept here for the process's life.
struct Sparkle {
    controller: Retained<AnyObject>,
    _delegate: Retained<Delegate>,
}

thread_local! {
    static START: Cell<Start> = const { Cell::new(Start::BeforeFirstPaint) };

    /// Whether this copy can update itself, once [`start`] has found
    /// out. Read by the menu's validation, which must not need the
    /// window's state.
    static AVAILABLE: Cell<bool> = const { Cell::new(false) };

    /// Sparkle, once a check has needed it.
    static SPARKLE: RefCell<Option<Sparkle>> = const { RefCell::new(None) };
}

/// The framework's binary in this bundle, set by [`start`] on a copy
/// that can update itself.
static FRAMEWORK: OnceLock<PathBuf> = OnceLock::new();

/// Whether the framework is loaded, decided by the first thread that
/// needs it.
static LOADED: OnceLock<bool> = OnceLock::new();

/// `updater.xml`, read by the first thread that needs it; `None` where
/// there is no configuration folder to keep it in.
static STORE: OnceLock<Option<StateStore>> = OnceLock::new();

/// Called after each paint of the editor. The first queues [`start`];
/// every later one costs one comparison.
///
/// It only queues: the notification arrives from inside Scintilla,
/// possibly below a frame that has the window's state borrowed.
pub(crate) fn after_paint() {
    if START.with(|start| start.replace(Start::Asked)) == Start::BeforeFirstPaint {
        on_main("updater:start", start);
    }
}

/// Whether this copy can update itself, as far as is known yet.
pub(crate) fn available() -> bool {
    AVAILABLE.with(Cell::get)
}

/// Run `f` on the main thread once its run loop is back in its default
/// mode, behind the callback boundary. Callable from any thread.
///
/// The default mode, and not the main queue alone, whose blocks also
/// run inside a modal dialog and while a menu or a drag is tracked:
/// a scheduled check must not put Sparkle's window over a prompt the
/// user is answering, and none of this may run below a frame that has
/// the window's state borrowed. The main queue is only the way onto
/// the main thread, where the run loop is then asked. The same shape
/// as `drain_in_default_mode`.
fn on_main(entry: &'static str, f: fn()) {
    DispatchQueue::main().exec_async(move || {
        let block = RcBlock::new(move || crate::at_callback_boundary(entry, (), f));
        // SAFETY: an extern static AppKit initialises before any code
        // of ours runs, read on the main thread.
        let default_mode: &NSString = unsafe { NSDefaultRunLoopMode };
        let modes = NSArray::from_slice(&[default_mode]);
        // SAFETY: the block is queued on the main run loop from the
        // main thread, so it runs there, and captures a `fn()` only.
        unsafe { NSRunLoop::mainRunLoop().performInModes_block(&modes, &block) };
    });
}

/// Run `f` on a thread of its own, which nothing waits for.
fn off_main(f: fn()) {
    if let Err(error) = std::thread::Builder::new().name("updater".into()).spawn(f) {
        tracing::warn!(error = ?error.to_string(), "the updater's thread could not be started");
    }
}

/// Whether the main bundle's `Info.plist` has text under `key`.
fn bundle_names(key: &NSString) -> bool {
    NSBundle::mainBundle()
        .objectForInfoDictionaryKey(key)
        .is_some_and(|value| {
            value
                .downcast_ref::<NSString>()
                .is_some_and(|s| s.length() > 0)
        })
}

/// Where the framework's binary would be in this bundle, if the bundle
/// names a feed and a key.
fn framework_in_bundle() -> Option<PathBuf> {
    if !bundle_names(ns_string!("SUFeedURL")) || !bundle_names(ns_string!("SUPublicEDKey")) {
        return None;
    }
    let frameworks = NSBundle::mainBundle().privateFrameworksPath()?;
    Some(PathBuf::from(frameworks.to_string()).join(FRAMEWORK_BINARY))
}

/// Establish whether this copy can update itself and, if it can, start
/// the schedule. Runs once, on the main thread, after the first paint.
///
/// The `Info.plist` is in memory already, and the one thing asked of
/// the disk is whether a file in the application's own bundle exists.
fn start() {
    // A guest instance (`--new-instance`) never updates itself: the
    // instance it was started from does, and two Sparkles on one bundle
    // would race over it. "Update Code++" stays greyed.
    if codepp_shell::is_guest_instance() {
        tracing::debug!("no updater: this is a guest instance");
        return;
    }
    let Some(framework) = framework_in_bundle().filter(|path| path.is_file()) else {
        tracing::debug!("no updater: this copy has no feed, no key or no Sparkle.framework");
        return;
    };
    if FRAMEWORK.set(framework).is_err() {
        return;
    }
    AVAILABLE.with(|available| available.set(true));
    let hourly = RcBlock::new(|_timer: NonNull<NSTimer>| {
        crate::at_callback_boundary("updater:tick", (), tick);
    });
    // SAFETY: called on the main thread, whose run loop keeps the timer;
    // the block touches only this module's main-thread state.
    let _timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(RECHECK_EVERY_SECS, true, &hourly)
    };
    tick();
}

/// Ask, off the main thread, whether a check is due, and make it if so.
/// Called at the start, every hour, and when the preferences change
/// ([`preferences_changed`]).
/// Does nothing on a copy that cannot update itself, with the
/// Auto-updater preference off, or once the quit has begun.
pub(crate) fn tick() {
    if !available() || crate::quitting() {
        return;
    }
    // The shell learns here that this copy can update itself, which is
    // what un-greys the preference (§11.5). Every tick says so again,
    // so one whose read of the state was declined is made up for by
    // the next: the hourly one, or the one the preferences make.
    let enabled = with_state(|st| {
        st.shell.set_can_self_update(true);
        st.shell.preferences.misc.auto_updater
    });
    if enabled != Some(true) {
        return;
    }
    off_main(|| {
        let due =
            store().is_some_and(|store| update::check_is_due(true, store.last_check(), now()));
        if due && load_framework() {
            on_main("updater:check", || check(Check::Background));
        }
    });
}

/// The preferences changed: ask again whether a check is due, so that
/// "Enable" takes effect now and not at the next hour. Through
/// [`on_main`], like every other tick but the hourly timer's, so it
/// never runs below the dialog that changed them.
pub(crate) fn preferences_changed() {
    on_main("updater:tick", tick);
}

/// "?" → "Update Code++": check now, with Sparkle's window.
pub(crate) fn check_now() {
    if !available() || crate::quitting() {
        return;
    }
    off_main(|| {
        if load_framework() {
            on_main("updater:checkNow", || check(Check::WithWindow));
        } else {
            on_main("updater:checkNow", could_not_start);
        }
    });
}

/// Tell the user that the check they asked for was not made.
fn could_not_start() {
    if crate::quitting() {
        return;
    }
    crate::queue_error_dialog(
        DIALOG_TITLE,
        "The updater could not be started, so no check was made.",
    );
}

/// The state file, read on first use. Never called on the main thread.
fn store() -> Option<&'static StateStore> {
    STORE
        .get_or_init(|| codepp_platform::updater_xml_path().map(StateStore::load))
        .as_ref()
}

/// Load the framework, once; later calls answer what the first found,
/// a failure included: a framework that would not load is not tried
/// again until the next start, and a check asked for meanwhile says
/// that the updater could not be started.
/// Never called on the main thread: the load reads the library from
/// disk and runs its initialisers.
fn load_framework() -> bool {
    *LOADED.get_or_init(|| {
        let Some(path) = FRAMEWORK.get() else {
            return false;
        };
        match DynLib::load(path) {
            Ok(library) => {
                // Never unmapped: its classes are registered with the
                // runtime and its objects live until the process ends.
                std::mem::forget(library);
                tracing::info!("the updater's framework is loaded");
                true
            }
            Err(error) => {
                tracing::warn!(error, "Sparkle.framework could not be loaded");
                false
            }
        }
    })
}

/// Ask Sparkle for a check. Main thread, with the framework loaded.
fn check(kind: Check) {
    if crate::quitting() {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let controller = SPARKLE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = start_sparkle(mtm);
        }
        slot.as_ref().map(|sparkle| sparkle.controller.clone())
    });
    let Some(controller) = controller else {
        if kind == Check::WithWindow {
            could_not_start();
        }
        return;
    };
    // SAFETY: `controller` is an `SPUStandardUpdaterController`, whose
    // `checkForUpdates:` takes a nullable sender and whose `updater`
    // answers the `SPUUpdater` that has `checkForUpdatesInBackground`;
    // both are main-thread calls, and this is the main thread.
    unsafe {
        match kind {
            Check::WithWindow => {
                let _: () = msg_send![&*controller, checkForUpdates: Option::<&AnyObject>::None];
            }
            Check::Background => {
                let updater: Option<Retained<AnyObject>> = msg_send![&*controller, updater];
                if let Some(updater) = updater {
                    let _: () = msg_send![&*updater, checkForUpdatesInBackground];
                }
            }
        }
    }
}

/// Make Sparkle's controller, which starts its updater. `None` if the
/// framework's class is not there, which a loaded framework rules out.
fn start_sparkle(mtm: MainThreadMarker) -> Option<Sparkle> {
    let Some(class) = AnyClass::get(c"SPUStandardUpdaterController") else {
        tracing::warn!("Sparkle is loaded but has no SPUStandardUpdaterController");
        return None;
    };
    let delegate = Delegate::new(mtm);
    // SAFETY: `initWithStartingUpdater:updaterDelegate:userDriverDelegate:`
    // is the class's designated initialiser: a BOOL and two nullable
    // objects, of which the updater's delegate is held weakly, so
    // `Sparkle` keeps it alive beside the controller.
    let controller: Option<Retained<AnyObject>> = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![class, alloc];
        msg_send![
            allocated,
            initWithStartingUpdater: true,
            updaterDelegate: &*delegate,
            userDriverDelegate: Option::<&AnyObject>::None,
        ]
    };
    Some(Sparkle {
        controller: controller?,
        _delegate: delegate,
    })
}

/// A check got an answer: remember when, so that the next is due a day
/// from now. A check that failed is not remembered, and is made again
/// at the next hour and the next start, as on Windows.
fn check_was_answered() {
    off_main(|| {
        if let Some(store) = store() {
            store.set_last_check(now());
        }
    });
}

define_class!(
    // SAFETY: a plain `NSObject` subclass with no ivars. Sparkle calls
    // its delegate on the main thread, and the methods touch nothing but
    // this module's statics.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppUpdaterDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    /// The two `SPUUpdaterDelegate` methods that say a check reached
    /// the feed and read it. The protocol's methods are all optional
    /// and Sparkle asks `respondsToSelector:`, so it is not adopted
    /// formally, which would need the framework's headers at build time.
    impl Delegate {
        #[unsafe(method(updater:didFindValidUpdate:))]
        fn did_find_valid_update(&self, _updater: &AnyObject, _item: &AnyObject) {
            crate::at_callback_boundary("updater:didFind", (), check_was_answered);
        }

        #[unsafe(method(updaterDidNotFindUpdate:error:))]
        fn did_not_find_update(&self, _updater: &AnyObject, _error: Option<&AnyObject>) {
            crate::at_callback_boundary("updater:didNotFind", (), check_was_answered);
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: `init` on a freshly allocated `NSObject` subclass.
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_paint_asks() {
        // Each test thread has its own state. What `after_paint` queues
        // never runs here: nothing runs this process's main queue.
        assert!(START.with(Cell::get) == Start::BeforeFirstPaint);
        after_paint();
        assert!(START.with(Cell::get) == Start::Asked);
        // It queues only on the change from "no paint yet", and
        // nothing else in the module moves that state.
        let after_paint = body("after_paint");
        let first = after_paint
            .find("if START.with(|start| start.replace(Start::Asked)) == Start::BeforeFirstPaint {")
            .expect("after_paint no longer asks whether this is the first paint");
        let queued = after_paint
            .find("on_main(")
            .expect("after_paint queues nothing");
        assert!(first < queued);
        assert_eq!(
            code().matches("Start::Asked").count(),
            1,
            "one place sets it"
        );
    }

    #[test]
    fn until_a_copy_is_known_to_update_itself_nothing_is_loaded() {
        // Before `start` has found a feed, a key and the framework, a
        // tick and a manual check do nothing. (`start` itself is not
        // run here: `NSBundle.mainBundle` took minutes to answer in a
        // test process on the machine this was written on.)
        assert!(!available());
        tick();
        check_now();
        assert!(FRAMEWORK.get().is_none());
        assert!(LOADED.get().is_none());
    }

    /// The source of this module up to its tests, without comment lines.
    fn code() -> String {
        let src = include_str!("updater.rs");
        let end = src.find("#[cfg(test)]").unwrap_or(src.len());
        src[..end]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The body of `fn name` in `code`: from its signature to the next
    /// item at the same depth.
    fn body_in(code: &str, name: &str) -> String {
        let start = code
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("no fn {name}"));
        let rest = &code[start..];
        let end = rest.find("\n}\n").map_or(rest.len(), |at| at + 2);
        rest[..end].to_owned()
    }

    /// The body of `fn name` in this module.
    fn body(name: &str) -> String {
        body_in(&code(), name)
    }

    /// The text between the brace that follows `opener` in `text` and
    /// the brace that closes it.
    fn braced<'a>(text: &'a str, opener: &str) -> &'a str {
        let from = text.find(opener).unwrap_or_else(|| panic!("no {opener}"));
        let open = from + text[from..].find('{').expect("no brace after the opener");
        let mut depth = 0_usize;
        for (at, c) in text[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &text[open + 1..open + at];
                    }
                }
                _ => {}
            }
        }
        panic!("the brace after {opener} is never closed");
    }

    #[test]
    fn braced_takes_exactly_what_the_braces_hold() {
        let text = "a(|| { b { c } d }); e { f }";
        assert_eq!(braced(text, "a(|| "), " b { c } d ");
        assert_eq!(braced(text, "e "), " f ");
    }

    #[test]
    fn the_framework_is_loaded_in_one_place_and_never_on_the_main_thread() {
        // §11.2: the load runs off the UI thread, and only when a check
        // needs it. One `DynLib::load`, in `load_framework`, which is
        // called twice, each time inside the closure handed to
        // `off_main`. The state file is read there and nowhere else.
        let code = code();
        assert_eq!(code.matches("DynLib::load(").count(), 1);
        assert!(body("load_framework").contains("DynLib::load("));
        assert_eq!(
            code.matches("load_framework()").count(),
            3,
            "its definition and two calls"
        );
        assert_eq!(
            code.matches("store()").count(),
            3,
            "its definition and two calls"
        );
        for caller in ["tick", "check_now"] {
            let body = body(caller);
            assert_eq!(body.matches("off_main(").count(), 1, "{caller}");
            let thread = braced(&body, "off_main(|| ");
            assert!(
                thread.contains("load_framework()"),
                "{caller} loads outside its thread"
            );
            let outside = body.replace(thread, "");
            assert!(
                !outside.contains("load_framework()"),
                "{caller} loads outside its thread"
            );
            assert!(
                !outside.contains("store()"),
                "{caller} reads updater.xml outside its thread"
            );
        }
        assert!(braced(&body("check_was_answered"), "off_main(|| ").contains("store()"));
        // And both do nothing on a copy that cannot update itself, or
        // once the quit has begun, before anything else.
        for guarded in ["tick", "check_now"] {
            let guard = "{\n    if !available() || crate::quitting() {\n        return;\n    }";
            assert!(body(guarded).contains(guard), "{guarded} lost its guard");
        }
    }

    #[test]
    fn a_scheduled_check_is_made_only_when_one_is_due() {
        let tick = body("tick");
        let due = tick.find("check_is_due(").expect("tick does not ask");
        let load = tick.find("load_framework()").expect("tick does not load");
        assert!(
            due < load,
            "the framework is loaded before a check is known to be due"
        );
        assert!(tick.contains("if due && load_framework()"));
        assert!(tick.contains("if enabled != Some(true)"));
    }

    #[test]
    fn nothing_of_the_updater_runs_before_the_first_paint() {
        // `after_paint` is the one way in besides the menu entry and
        // the preferences, and it only queues; and the paint arm of the
        // editor's notifications is the one place that calls it.
        assert!(body("after_paint").contains("on_main(\"updater:start\", start)"));
        // `start` is private, and nothing in the module calls it: it is
        // only ever handed to `on_main`, there.
        let code = code();
        let calls = code
            .match_indices("start()")
            .filter(|(at, _)| {
                let before = code[..*at].chars().next_back();
                !before.is_some_and(|c| c == '_' || c.is_alphanumeric())
            })
            .count();
        assert_eq!(calls, 1, "`fn start()` itself, and no call of it");
        assert_eq!(code.matches(", start)").count(), 1);
        let lib: String = include_str!("lib.rs")
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(lib.matches("updater::after_paint()").count(), 1);
        let notify = body_in(&lib, "on_sci_notify_inner");
        let arm = braced(&notify, "codepp_scintilla_sys::SCN_PAINTED => ");
        assert!(
            arm.contains("updater::after_paint();"),
            "not in the paint arm"
        );
    }

    #[test]
    fn what_runs_on_the_main_thread_waits_for_its_default_mode() {
        // A scheduled check must not open Sparkle's window over a modal
        // prompt, nor run below a borrow of the window's state: `on_main`
        // is the one way onto the main thread, and it asks the run loop
        // for its default mode.
        let code = code();
        assert_eq!(code.matches("exec_async(").count(), 1);
        let on_main = body("on_main");
        assert!(on_main.contains("exec_async("));
        assert!(on_main.contains("NSDefaultRunLoopMode"));
        assert!(on_main.contains("performInModes_block(&modes, &block)"));
        assert!(on_main.contains("crate::at_callback_boundary(entry, (), f)"));
    }
}
