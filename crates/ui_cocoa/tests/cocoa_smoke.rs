//! The DESIGN.md §5.6 smoke test for the Cocoa backend: build a real
//! Scintilla view, capture the §4.2 direct-call pair, and drive text
//! through it.
//!
//! # Why `harness = false`
//!
//! AppKit is main-thread-only, and **libtest never yields the main
//! thread** — it runs every `#[test]` on a spawned worker, including at
//! `--test-threads=1` (measured, not assumed: the first version of this
//! test lived in `src/lib.rs` and its `MainThreadMarker::new()` guard
//! failed under exactly that invocation). `ui_gtk` gets away with a
//! normal `#[test]` because GTK only requires that all its calls happen
//! on *one* thread, not specifically the first; AppKit is stricter.
//!
//! Setting `harness = false` in `Cargo.toml` replaces libtest with this
//! file's own `main`, which cargo runs directly — on the real main
//! thread. That is the only way to drive AppKit from `cargo test`.
//!
//! # Why it is opt-in
//!
//! It needs a window server, so it mirrors the `#[ignore]` convention
//! the GTK display tests use (`DEVELOPMENT.md` §3.3) — skipped by
//! default, run explicitly:
//!
//! ```sh
//! cargo test -p codepp-ui-cocoa --test cocoa_smoke -- --ignored
//! ```
//!
//! Without a harness there is no `#[ignore]` attribute to lean on, so
//! the flag is parsed by hand below and the default path prints a
//! libtest-shaped summary naming the ignored test. That is deliberate:
//! DESIGN.md §7.4 records that a *silent* runtime skip lets a runner in
//! the wrong state drop coverage while reporting green, so the skip has
//! to be visible in the output.
//!
//! # Why `main` is not cfg-gated away
//!
//! The macOS-only body lives in an inner `#[cfg]` module and there are
//! two `main`s, rather than one file-level `#![cfg(target_os = "macos")]`.
//! That shape is required, not stylistic: `harness = false` makes cargo
//! compile this file as a `bin`-type crate, so it must have a real
//! `main` on **every** target. Gating the whole file away leaves a crate
//! with no items at all, which is `error[E0601]: main function not found`
//! — a hard compile error, not an empty test binary. Verified against
//! `--target x86_64-unknown-linux-gnu`, and it would have broken the
//! `linux` and `windows` CI runners, which run
//! `cargo test --workspace --all-targets` (DESIGN.md §9.3).
//!
//! The gate cannot simply be deleted either: `objc2*` and the rest are
//! declared under `[target.'cfg(target_os = "macos")'.dependencies]`, so
//! their `use` statements do not resolve elsewhere. Hence: gate the
//! body, keep a `main` unconditionally.
//!
//! No other crate in the workspace hits this, because no other crate
//! sets `harness = false` — an ordinary `#[test]` gets libtest's
//! synthesized `main` for free even when every item is cfg'd out.

#[cfg(target_os = "macos")]
mod smoke {
    use codepp_editor::EditorHandle;
    use codepp_scintilla_sys::{
        scintilla_cocoa_new, scintilla_cocoa_set_notify_callback, sptr_t, Sci_NotifyHeader,
        COCOA_WM_NOTIFY, SCI_CANUNDO, SCI_GETLENGTH, SCI_GETSELECTIONEND, SCI_GETSELECTIONSTART,
        SCI_REDO, SCI_SELECTALL, SCI_SETTEXT, SCI_UNDO, SCN_SAVEPOINTLEFT, SCN_UPDATEUI,
    };
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;
    use std::ffi::CString;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TEST_TEXT: &str = "hello from cocoa";

    pub fn run() {
        let opted_in = std::env::args().any(|a| a == "--ignored" || a == "--include-ignored");
        if !opted_in {
            println!(
                "\nrunning 0 tests\n\n\
             test result: ok. 0 passed; 0 failed; 1 ignored\n\n\
             note: `cocoa_smoke` needs a window server and is ignored by default.\n\
             run it with: cargo test -p codepp-ui-cocoa --test cocoa_smoke -- --ignored\n"
            );
            return;
        }

        println!("\nrunning 4 tests");

        // Hard failure rather than a skip: with `harness = false` this *is*
        // the main thread, so `None` here would mean cargo changed how it
        // invokes test binaries — worth failing loudly over.
        let mtm = MainThreadMarker::new()
            .expect("not on the main thread — `harness = false` should guarantee it");

        // `scintilla_cocoa_new`'s documented precondition.
        let _app = NSApplication::sharedApplication(mtm);

        direct_call_round_trip();
        println!("test cocoa_smoke::direct_call_round_trip ... ok");

        notifications_are_delivered();
        println!("test cocoa_smoke::notifications_are_delivered ... ok");

        // The last two scenarios need `codepp_ui_cocoa::smoke_support`,
        // which is compiled out of release builds because it can rewrite
        // the plugin dispatcher's trust anchor. A `--release` test build
        // therefore reports them ignored rather than silently dropping
        // them.
        #[cfg(debug_assertions)]
        {
            marshal::a_plugins_worker_thread_reaches_scintilla_through_the_main_queue();
            println!(
                "test cocoa_smoke::a_plugins_worker_thread_reaches_scintilla_through_the_main_queue ... ok"
            );
            codepp_ui_cocoa::smoke_support::plugin_panels_are_hosted_by_the_dock();
            println!("test cocoa_smoke::plugin_panels_are_hosted_by_the_dock ... ok");
            println!("\ntest result: ok. 4 passed; 0 failed; 0 ignored\n");
        }
        #[cfg(not(debug_assertions))]
        {
            println!(
                "test cocoa_smoke::a_plugins_worker_thread_reaches_scintilla_through_the_main_queue ... ignored"
            );
            println!("test cocoa_smoke::plugin_panels_are_hosted_by_the_dock ... ignored");
            println!(
                "\ntest result: ok. 2 passed; 0 failed; 2 ignored\n\n\
                 note: the marshal and plugin-panel scenarios need a debug build \
                 (`smoke_support` is compiled out of release).\n"
            );
        }
    }

    /// Drive a real Scintilla view through the captured direct-call pair.
    ///
    /// Every assertion goes through `EditorHandle::send`, i.e. the captured
    /// function pointer — *not* `scintilla_cocoa_send_message`. That is the
    /// point of the test: it proves the fast path works, which is the
    /// substance of m1. A test written against the message-send path could
    /// pass while the direct-call capture was silently broken, and the
    /// direct-call path is the one every keystroke uses (DESIGN.md §4.2).
    fn direct_call_round_trip() {
        // SAFETY: `NSApplication` exists and this is the main thread — the
        // two documented preconditions.
        let sci_ptr = unsafe { scintilla_cocoa_new() };
        assert!(!sci_ptr.is_null(), "scintilla_cocoa_new() returned null");

        // SAFETY: `sci_ptr` is the live view just constructed.
        let editor = unsafe { EditorHandle::from_cocoa_view(sci_ptr) }
            .expect("Scintilla did not surrender its direct-call pair");

        // `cast_signed` rather than `as`: the lib crate carries a blanket
        // allow for Scintilla ABI casts, but an integration test is its own
        // crate and does not inherit it — and here the precise spelling is
        // free, since a 16-byte literal cannot wrap.
        let len: sptr_t = TEST_TEXT.len().cast_signed();

        // Fresh buffer is empty.
        assert_eq!(
            editor.send(SCI_GETLENGTH, 0, 0),
            0,
            "fresh buffer not empty"
        );

        // Insert and read back — a real round trip through the direct-call
        // pointer.
        let text = CString::new(TEST_TEXT).expect("no interior NUL");
        editor.send(SCI_SETTEXT, 0, text.as_ptr() as sptr_t);
        assert_eq!(
            editor.send(SCI_GETLENGTH, 0, 0),
            len,
            "SCI_SETTEXT did not reach the buffer through the direct-call path"
        );

        // Select All — where the Edit menu's `selectAll:` ends up.
        editor.send(SCI_SELECTALL, 0, 0);
        assert_eq!(editor.send(SCI_GETSELECTIONSTART, 0, 0), 0);
        assert_eq!(
            editor.send(SCI_GETSELECTIONEND, 0, 0),
            len,
            "SCI_SELECTALL did not cover the whole buffer"
        );

        // Undo, then redo — likewise what the Edit menu's `undo:` / `redo:`
        // reach, and the m1 demo's acceptance criteria.
        assert_eq!(
            editor.send(SCI_CANUNDO, 0, 0),
            1,
            "buffer reports nothing to undo after an edit"
        );
        editor.send(SCI_UNDO, 0, 0);
        assert_eq!(
            editor.send(SCI_GETLENGTH, 0, 0),
            0,
            "SCI_UNDO did not revert the insert"
        );
        editor.send(SCI_REDO, 0, 0);
        assert_eq!(
            editor.send(SCI_GETLENGTH, 0, 0),
            len,
            "SCI_REDO did not reapply the insert"
        );

        // The view is deliberately not released — see
        // `scintilla_cocoa_new`'s ownership contract in the shim. A backend
        // that never destroys views leaks exactly one per run here, which is
        // the intended behaviour rather than an oversight.
    }

    static UPDATEUI_SEEN: AtomicUsize = AtomicUsize::new(0);
    static SAVEPOINTLEFT_SEEN: AtomicUsize = AtomicUsize::new(0);
    static ANY_SEEN: AtomicUsize = AtomicUsize::new(0);

    /// SAFETY: matches `SciNotifyFunc`; called synchronously by
    /// Scintilla on the main thread with a live `SCNotification*` in
    /// `lparam` when `message` is `COCOA_WM_NOTIFY`.
    unsafe extern "C" fn counting_notify(_id: isize, message: u32, _w: usize, lparam: usize) {
        if message != COCOA_WM_NOTIFY || lparam == 0 {
            return;
        }
        // SAFETY: prefix read of the live notification, as documented on
        // `Sci_NotifyHeader`.
        let code = unsafe { (*(lparam as *const Sci_NotifyHeader)).code };
        ANY_SEEN.fetch_add(1, Ordering::Relaxed);
        match code {
            SCN_UPDATEUI => {
                UPDATEUI_SEEN.fetch_add(1, Ordering::Relaxed);
            }
            SCN_SAVEPOINTLEFT => {
                SAVEPOINTLEFT_SEEN.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// Prove Scintilla's notifications actually reach Rust.
    ///
    /// Without this the wiring is only proven by "the app did not
    /// crash", which is exactly the kind of evidence that let the m3a
    /// dirty-tracking bug ship-and-be-caught: code that compiled, ran,
    /// and silently did nothing. `SCN_UPDATEUI` is what makes the status
    /// bar track typing and `SCN_SAVEPOINTLEFT` is what drives the tab
    /// strip's dirty marker, so a regression in either is a
    /// user-visible, silent one.
    fn notifications_are_delivered() {
        // SAFETY: `NSApplication` exists and this is the main thread.
        let sci_ptr = unsafe { scintilla_cocoa_new() };
        assert!(!sci_ptr.is_null(), "scintilla_cocoa_new() returned null");

        // SAFETY: live view; the callback matches `SciNotifyFunc`.
        unsafe { scintilla_cocoa_set_notify_callback(sci_ptr, counting_notify, 0) };

        // SAFETY: `sci_ptr` is the live view just constructed.
        let editor = unsafe { EditorHandle::from_cocoa_view(sci_ptr) }
            .expect("Scintilla did not surrender its direct-call pair");

        let text = CString::new(TEST_TEXT).expect("no interior NUL");
        editor.send(SCI_SETTEXT, 0, text.as_ptr() as sptr_t);

        // The wiring itself: notifications reach Rust at all.
        assert!(
            ANY_SEEN.load(Ordering::Relaxed) > 0,
            "no Scintilla notifications reached Rust — the callback is \
             not wired, and the status bar and dirty marker would both \
             be permanently stale"
        );
        // The dirty edge that drives the tab strip's marker. Emitted
        // synchronously from the edit, so it is observable here.
        assert!(
            SAVEPOINTLEFT_SEEN.load(Ordering::Relaxed) > 0,
            "no SCN_SAVEPOINTLEFT after an edit — the tab strip's dirty \
             marker would never light up"
        );
        // `SCN_UPDATEUI` is deliberately **not** asserted, and the
        // reason is a test-harness limitation rather than a production
        // concern: it is emitted from `Editor::Paint`/`Idle`, which need
        // a running run loop and a view inside a window, and this test
        // has neither. Measured here as zero while four other
        // notifications arrived.
        //
        // That measurement is what motivated `on_sci_notify` handling
        // `SCN_MODIFIED` alongside `SCN_UPDATEUI` — the status bar
        // should not depend on paint timing. This test cannot observe
        // that arm firing (no run loop), so it asserts the wiring and
        // the synchronous edge instead of pretending otherwise.
    }

    /// The cross-thread `SCI_*` marshal scenario, in its own module so
    /// its imports — and its dependency on the crate's debug-only
    /// `smoke_support` surface — are gated with it.
    #[cfg(debug_assertions)]
    mod marshal {
        use codepp_editor::EditorHandle;
        use codepp_scintilla_sys::{scintilla_cocoa_new, sptr_t, SCI_GETLENGTH, SCI_SETTEXT};
        use codepp_ui_cocoa::smoke_support;
        use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};
        use std::ffi::{c_void, CString};
        use std::time::Duration;

        /// A raw view pointer handed to a worker thread.
        struct WorkerPtr(*mut c_void);
        // SAFETY: the test's own leaked, permanently-live Scintilla view. The
        // worker only hands it to `plugin_dispatch`, which is the code under
        // test and is precisely what must not dereference it off the main
        // thread.
        unsafe impl Send for WorkerPtr {}

        /// Pump the main run loop once, briefly, in the default mode.
        ///
        /// GCD's main queue is drained by the main thread's run loop, so
        /// this is what lets a `dispatch_sync` block a worker enqueued
        /// actually execute. The mode matters: the main queue is serviced
        /// only in a *common* mode, of which the default mode is one.
        fn pump_main_queue_once() {
            let deadline = NSDate::dateWithTimeIntervalSinceNow(0.01);
            // SAFETY: main thread (the harness guarantees it) and a live
            // mode constant.
            let _ = unsafe {
                NSRunLoop::mainRunLoop().runMode_beforeDate(NSDefaultRunLoopMode, &deadline)
            };
        }

        /// The DESIGN.md §7.4 cross-thread `SCI_*` marshal, driven for real.
        ///
        /// The Cocoa counterpart of `ui_gtk`'s
        /// `a_plugins_worker_thread_reaches_scintilla_through_the_main_loop`,
        /// and it lives *here* rather than in the crate's own test module
        /// because it needs the process main thread — which only this
        /// `harness = false` binary owns.
        ///
        /// Three claims, each of which has a way to pass vacuously that the
        /// test is shaped to exclude:
        ///
        ///   1. **Same thread → answered directly.** The control. It also
        ///      proves the affinity check is armed, so the cross-thread
        ///      assertion cannot pass by classifying *everything* as remote.
        ///      On this backend it is more than a control: a fast path that
        ///      wrongly marshals from the main thread is a `dispatch_sync`
        ///      onto the queue the caller is standing on, which libdispatch
        ///      answers by aborting the process — so that mutation shows up
        ///      as a `SIGTRAP` (exit 133) with "`dispatch_sync` called on
        ///      queue already owned by current thread" in the crash report,
        ///      rather than as a failed assertion. Measured.
        ///   2. **Other thread → parked until the main queue drains.** The
        ///      call must still be outstanding after 200 ms of *not* pumping
        ///      the run loop; a regressed direct call finishes in
        ///      microseconds, so a starved runner can only lengthen the
        ///      wait, never turn a real failure into a pass.
        ///   3. **Other thread → the real answer once it does.** Five bytes
        ///      are seeded so a correct round trip has a distinctive value —
        ///      `0` is what every failure mode returns.
        ///
        /// And the identity check still runs *before* the affinity check: a
        /// bogus handle from a worker is refused, not marshaled.
        pub(super) fn a_plugins_worker_thread_reaches_scintilla_through_the_main_queue() {
            // SAFETY: `NSApplication` exists and this is the main thread.
            let sci_ptr = unsafe { scintilla_cocoa_new() };
            assert!(!sci_ptr.is_null(), "scintilla_cocoa_new() returned null");
            // SAFETY: a view from `scintilla_cocoa_new` that this test
            // never releases — the same leak-by-design as the other two
            // scenarios — so it stays live for the rest of the process.
            unsafe { smoke_support::arm_scintilla(sci_ptr) };

            // SAFETY: `sci_ptr` is the live view just constructed.
            let editor = unsafe { EditorHandle::from_cocoa_view(sci_ptr) }
                .expect("Scintilla did not surrender its direct-call pair");
            let text = CString::new("hello").expect("no interior NUL");
            editor.send(SCI_SETTEXT, 0, text.as_ptr() as sptr_t);

            // 1. The control.
            assert_eq!(
                smoke_support::dispatch(sci_ptr, SCI_GETLENGTH, 0, 0),
                5,
                "same-thread SCI_* must answer directly"
            );

            // 2. The real case.
            let handle = WorkerPtr(sci_ptr);
            let worker = std::thread::spawn(move || {
                let handle = handle;
                smoke_support::dispatch(handle.0, SCI_GETLENGTH, 0, 0)
            });
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                !worker.is_finished(),
                "a cross-thread SCI_* answered without the main queue being drained — \
                 it was executed on the calling thread, which is the bug"
            );

            // 3. ...and answered once the main thread gets to it.
            let mut spins = 0;
            while !worker.is_finished() {
                pump_main_queue_once();
                spins += 1;
                assert!(spins < 10_000, "marshaled SCI_* never completed");
            }
            assert_eq!(
                worker.join().expect("worker panicked"),
                5,
                "the marshaled call must return Scintilla's real answer"
            );

            // An unrecognised handle is refused rather than marshaled, from
            // a worker just as from the main thread.
            let bogus = WorkerPtr(std::ptr::dangling_mut::<u8>().cast::<c_void>());
            let refused = std::thread::spawn(move || {
                let bogus = bogus;
                smoke_support::dispatch(bogus.0, SCI_GETLENGTH, 0, 0)
            });
            assert_eq!(
                refused.join().expect("worker panicked"),
                0,
                "an unknown handle must be refused without dereferencing it"
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    smoke::run();
}

/// Non-macOS `main`. See the "Why `main` is not cfg-gated away" section
/// in this file's module docs: `harness = false` makes cargo build this
/// as a `bin`, which needs a `main` on every target even where the test
/// itself cannot exist.
#[cfg(not(target_os = "macos"))]
fn main() {
    println!(
        "\nrunning 0 tests\n\n\
         test result: ok. 0 passed; 0 failed; 0 ignored\n\n\
         note: `cocoa_smoke` is macOS-only and is not built on this target.\n"
    );
}
