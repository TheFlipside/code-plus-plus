// ScintillaCocoaShim.mm — C entry points over Scintilla's Cocoa backend.
//
// Why this file exists.
//   The other two backends hand Code++ a C surface for free. Win32
//   registers a window class (`Scintilla_RegisterClasses`) and the host
//   calls `CreateWindowExW`; GTK exports a `scintilla_new()` factory plus
//   `scintilla_send_message()` from `gtk/ScintillaGTK.cxx`. The Cocoa
//   backend exports neither: `cocoa/ScintillaView.h` declares only
//   `@interface ScintillaView : NSView`, and the single C-linkage symbol
//   in the whole `cocoa/` tree is `NSString *ScintillaRecPboardType`.
//   Objective-C methods are not callable from Rust, so this file supplies
//   the missing pair, deliberately mirroring GTK's names and signatures
//   so `crates/editor` stays backend-agnostic — `EditorHandle` captures
//   the same `(fn_ptr, instance_ptr)` direct-call pair either way
//   (DESIGN.md §4.2).
//
// ARC.
//   This file is compiled with `-fobjc-arc` because the vendored tree
//   requires it: `cocoa/ScintillaView.mm:29` is a literal
//   `#if !__has_feature(objc_arc)` / `#error ARC must be enabled`, and
//   `cocoa/PlatCocoa.mm` is written throughout in `__bridge` casts. That
//   is why `scintilla_cocoa_new` returns through `__bridge_retained`
//   rather than a plain cast: it transfers a +1 reference out of ARC's
//   control to the caller, the same idiom the vendored code itself uses
//   at `PlatCocoa.mm:1989`. A plain `(__bridge void *)` would hand back a
//   pointer ARC releases at the end of this function, and the caller
//   would be holding freed memory on its very first message.
//
// Ownership contract (read this before adding a destroy entry point).
//   `scintilla_cocoa_new` returns an owned +1 reference and there is
//   deliberately no `scintilla_cocoa_release`. Code++ creates its
//   Scintilla views once at startup and never destroys, removes or
//   reassigns them, switching buffers underneath a permanent view with
//   `SCI_SETDOCPOINTER` instead (DESIGN.md §7.2, §7.4). That is not a
//   simplification to be tidied up later — it is what makes
//   `EditorHandle` sound. `EditorHandle` is `Copy`, has no `Drop`, and
//   stores raw pointers into the view without expressing a lifetime, so
//   a copy outliving the view would leave `direct_ptr` dangling and the
//   next direct call would fault inside vendored C++ rather than fail a
//   check. Adding a release path here would hand callers the one tool
//   needed to violate that. See `crates/editor/src/lib.rs`'s safety
//   documentation on `from_cocoa_view`.
//
// Provenance.
//   No source is copied from upstream Scintilla. The two entry-point
//   bodies are written here from scratch against the public interface
//   declared in `vendor/scintilla/cocoa/ScintillaView.h` — `initWithFrame:`
//   (:1451 in ScintillaView.mm) and `message:wParam:lParam:`
//   (ScintillaView.h:121). Same discipline as `LexillaShim.cxx`.

#import "ScintillaView.h"

// `Scintilla.h`, imported transitively by `ScintillaView.h`, supplies
// `sptr_t` and `uptr_t`. They are the pointer-sized integers Scintilla's
// whole message ABI is expressed in, and match `codepp_scintilla_sys`'s
// `sptr_t = isize` / `uptr_t = usize` on every target we build.

// The frame a freshly constructed view is given. Arbitrary and
// temporary: the host resizes the view to its container as soon as it is
// added, on every backend. It is not zero because `ScintillaView`'s
// designated initialiser builds an `NSScrollView` and a margin view
// against this rect, and a degenerate frame there is a needless edge
// case to hand to vendored layout code on the very first paint.
static const CGFloat kInitialWidth = 800.0;
static const CGFloat kInitialHeight = 600.0;

extern "C" {

/// Construct a `ScintillaView`. Returns an owned +1 reference the caller
/// keeps for the lifetime of the process, or `nullptr` if AppKit refused
/// to build the view.
///
/// The Cocoa counterpart of GTK's `scintilla_new()`. Unlike GTK's — which
/// returns a *floating* GObject reference that the container sinks —
/// this hands back a genuinely owned reference, because `NSView` has no
/// equivalent of floating references. The caller adds it to a superview,
/// which retains it independently.
///
/// Must be called on the main thread, after `NSApplication` exists.
/// AppKit view construction is main-thread-only, and this builds an
/// `NSScrollView` and two subviews.
///
/// **Why the catch, and exactly what it does and does not guarantee.**
/// `-[ScintillaView initWithFrame:]` (ScintillaView.mm:1451) does a bare
/// `mBackend = new ScintillaCocoa(...)` with no handler of its own. This
/// differs from the GTK backend, whose `scintilla_init` wraps
/// construction in `catch (...)` and returns a well-formed widget with a
/// null interior — a worse outcome, but a contained one. Here an
/// allocation failure during backend setup would instead propagate out
/// through this `extern "C"` boundary and unwind into Rust, which is
/// undefined behaviour and in practice an abort.
///
/// What the `@catch` guarantees: nothing unwinds into Rust. On 64-bit
/// Apple platforms Objective-C and C++ share one zero-cost exception
/// model, so `@catch (...)` genuinely catches a C++ `std::bad_alloc` as
/// well as an `NSException`. That is the property this exists for.
///
/// What it does **not** guarantee is clean teardown of the half-built
/// view. The throw originates inside vendored `ScintillaView.mm`, a
/// separate translation unit; `-fobjc-arc-exceptions` is passed to that
/// builder (see `build_scintilla_cocoa`) so ARC emits unwind cleanup for
/// its locals, but the partially-initialised `ScintillaView` and any
/// subviews it had already installed are still most likely leaked rather
/// than released. Trading a leak for a defined failure mode is the right
/// way round, and on a path that needs local memory exhaustion at
/// window-construction time it is close to academic — but it should not
/// be read as "safe teardown".
///
/// The `NSLog` is deliberate: without it a genuine programming-error
/// `NSException` (as opposed to OOM) would be swallowed into an
/// indistinguishable null return, leaving no trail. An uncaught
/// exception would at least have produced a crash report.
void *scintilla_cocoa_new(void) {
	@try {
		ScintillaView *view = [[ScintillaView alloc]
				       initWithFrame: NSMakeRect(0.0, 0.0,
								 kInitialWidth, kInitialHeight)];
		if (view == nil) {
			return nullptr;
		}
		return (__bridge_retained void *)view;
	} @catch (NSException *e) {
		NSLog(@"Code++: ScintillaView construction raised %@: %@", e.name, e.reason);
		return nullptr;
	} @catch (...) {
		// A non-NSException throw — realistically `std::bad_alloc`
		// from `new ScintillaCocoa`. Nothing structured to report.
		NSLog(@"Code++: ScintillaView construction threw a non-NSException");
		return nullptr;
	}
}

/// Send a Scintilla message to a view returned by `scintilla_cocoa_new`.
///
/// The Cocoa counterpart of GTK's `scintilla_send_message()`, and like it
/// this is the *slow* path — reserved for setup and for the two
/// direct-call capture messages (`SCI_GETDIRECTFUNCTION` /
/// `SCI_GETDIRECTPOINTER`). Every hot path goes through the captured
/// function pointer instead, per DESIGN.md §4.2.
///
/// `view` must be a live pointer from `scintilla_cocoa_new`. The null
/// check below catches only the null case; a dangling or foreign pointer
/// is undefined behaviour, exactly as it is on the GTK side, because the
/// bridge cast cannot validate what it is handed. (GTK's
/// `scintilla_send_message` does not even null-check, so this is
/// marginally stricter than its sibling, not weaker.)
///
/// **No exception handler here, and that is a considered decision that
/// depends on vendored code.** This forwards into arbitrary Scintilla
/// message handling, which in isolation would mean an exception could
/// unwind across this `extern "C"` boundary into Rust. It cannot today
/// because `ScintillaCocoa::WndProc` (`cocoa/ScintillaCocoa.mm:895-966`)
/// wraps its entire dispatch in `catch (std::bad_alloc &)` plus
/// `catch (...)` and converts either into a status code — and both this
/// slow path and `ScintillaCocoa::DirectFunction` (the hot path every
/// keystroke uses) funnel through it. Adding a redundant `@try` here
/// would suggest the hot path is protected by something in this file,
/// which it is not.
///
/// The dependency is worth naming because it is invisible from here: if
/// a future Scintilla bump narrows or removes that catch-all, this call
/// site and the direct-call path both silently regain the risk with no
/// local signal. Re-check `WndProc`'s structure when bumping the
/// submodule pin (DEVELOPMENT.md §5 already requires diffing the tree).
sptr_t scintilla_cocoa_send_message(void *view, unsigned int message,
				    uptr_t wparam, sptr_t lparam) {
	if (view == nullptr) {
		return 0;
	}
	ScintillaView *sv = (__bridge ScintillaView *)view;
	return [sv message: message wParam: wparam lParam: lparam];
}

/// Register a C callback to receive Scintilla's notifications.
///
/// **Why the deprecated API, deliberately.** `ScintillaView.h:144`
/// marks `registerNotifyCallback:value:` deprecated and points at the
/// `delegate` property instead. The delegate is the wrong tool here,
/// because the two are not equivalent:
///
///   * `ScintillaCocoa::NotifyParent` (`ScintillaCocoa.mm:2119`) calls
///     `notifyProc` **and** its delegate — they are additive.
///   * `ScintillaView` installs *itself* as the backend's delegate
///     (`ScintillaView.mm:1491`), and its `-notification:`
///     (`:1376`) does the built-in fold-margin click handling. But when
///     a *host* delegate is set, that method hands the notification over
///     and then `return`s for every code except Zoom and UpdateUI
///     (`:1379-1384`) — so setting `ScintillaView.delegate` silently
///     disables folding by margin click.
///
/// So the recommended replacement is destructive of behaviour Code++
/// wants, and the deprecated path is the non-destructive one. It is also
/// cheaper: a direct C call per notification rather than an
/// Objective-C message send, and `SCN_MODIFIED` fires on every
/// keystroke, which DESIGN.md §8 budgets at 5 ms p99.
///
/// If a future Scintilla bump removes the entry point, the migration is
/// to set the delegate *and* re-implement the margin-click fold toggle
/// host-side — not simply to swap one call for the other.
///
/// `windowid` is passed back to the callback verbatim; Code++ has one
/// view and passes 0.
void scintilla_cocoa_set_notify_callback(void *view, SciNotifyFunc callback,
					 intptr_t windowid) {
	if (view == nullptr) {
		return;
	}
	ScintillaView *sv = (__bridge ScintillaView *)view;
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
	[sv registerNotifyCallback: windowid value: callback];
#pragma clang diagnostic pop
}

} // extern "C"
