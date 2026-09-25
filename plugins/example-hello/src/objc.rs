//! The Objective-C runtime, reached directly: `objc_getClass`,
//! `sel_registerName` and `objc_msgSend`, cast per call to the method's
//! own prototype.
//!
//! example-hello's AppKit pieces — the dock panels' content, the toolbar
//! button's image and the modeless dialog — are a handful of messages
//! each, and a plugin a third party might copy as a starting point is
//! better off showing the dependency-free shape than a binding crate.
//! Only `libobjc` is linked. AppKit's classes are looked up by name, so a
//! process that has not loaded AppKit — a test harness — finds none and
//! builds nothing, where linking AppKit would have loaded it.

use core::ffi::{c_char, c_int, c_void, CStr};

/// An Objective-C object pointer, `id`.
pub(crate) type Id = *mut c_void;
/// A selector, `SEL`.
pub(crate) type Sel = *const c_void;

/// `BOOL`: `bool` on arm64, `signed char` on `x86_64`.
#[cfg(target_arch = "aarch64")]
pub(crate) type ObjcBool = bool;
/// `BOOL`: `bool` on arm64, `signed char` on `x86_64`.
#[cfg(not(target_arch = "aarch64"))]
pub(crate) type ObjcBool = i8;

/// `NO`, in whichever type `BOOL` is on this architecture.
#[cfg(target_arch = "aarch64")]
pub(crate) const NO: ObjcBool = false;
/// `NO`, in whichever type `BOOL` is on this architecture.
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const NO: ObjcBool = 0;

/// `CGPoint` / `CGSize` / `CGRect`, which `NSRect` is.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Point {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Size {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Rect {
    origin: Point,
    size: Size,
}

/// An `NSRect`.
pub(crate) fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
    Rect {
        origin: Point { x, y },
        size: Size { width, height },
    }
}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    /// Never called as declared: each use is cast to the prototype of
    /// the method it sends — see [`send`].
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

extern "C" {
    /// `libSystem`: nonzero on the process's main thread.
    fn pthread_main_np() -> c_int;
}

/// `objc_msgSend` as the prototype `F` of the method being sent.
///
/// On arm64 the untyped symbol cannot be called as declared: a variadic
/// call passes its arguments differently from the method's own prototype.
/// So every send goes through an exact function type, and none of them
/// returns a structure — which on `x86_64` would need
/// `objc_msgSend_stret` instead.
///
/// # Safety
///
/// `F` must be an `unsafe extern "C" fn(Id, Sel, …)` type matching the
/// method it will be called with, argument for argument.
pub(crate) unsafe fn send<F: Copy>() -> F {
    // Catches an `F` that is not a function pointer at all — a value type
    // passed by mistake. It cannot catch the wrong prototype: every
    // function pointer is one word, so that stays the caller's contract
    // above.
    debug_assert_eq!(
        core::mem::size_of::<F>(),
        core::mem::size_of::<unsafe extern "C" fn()>(),
        "`send` casts a function pointer to a function pointer, nothing else"
    );
    // SAFETY: a function pointer, reinterpreted as another function
    // pointer of the same size — the caller's contract makes the
    // signature the method's own.
    unsafe { core::mem::transmute_copy::<unsafe extern "C" fn(), F>(&(objc_msgSend as _)) }
}

/// The selector named `name`.
pub(crate) fn sel(name: &CStr) -> Sel {
    // SAFETY: registers or finds a selector by its NUL-terminated name;
    // never fails.
    unsafe { sel_registerName(name.as_ptr()) }
}

/// The class named `name`, or null when the process has not loaded it.
pub(crate) fn class(name: &CStr) -> Id {
    // SAFETY: looks a class up by its NUL-terminated name; answers null
    // for one that is not loaded.
    unsafe { objc_getClass(name.as_ptr()) }
}

/// Whether AppKit is up to build with: this is the main thread, and the
/// process has loaded AppKit. Always so inside the Cocoa host; a host
/// that loads plugins elsewhere — a test harness on a worker thread —
/// gets nothing built, since AppKit's views are main-thread only.
pub(crate) fn ready() -> bool {
    // SAFETY: reads process state and takes nothing.
    unsafe { pthread_main_np() != 0 && !class(c"NSView").is_null() }
}

/// An `NSString` for `text`, autoreleased: valid until the innermost
/// [`with_pool`] around the call ends. Null if Foundation could not make
/// it.
pub(crate) fn string(text: &CStr) -> Id {
    let string_class = class(c"NSString");
    if string_class.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `stringWithUTF8String:`'s own prototype, sent to the class
    // looked up above with a NUL-terminated string.
    unsafe {
        let with_utf8: unsafe extern "C" fn(Id, Sel, *const c_char) -> Id = send();
        with_utf8(string_class, sel(c"stringWithUTF8String:"), text.as_ptr())
    }
}

/// Run `f` inside an autorelease pool, so the objects AppKit hands back
/// autoreleased — strings, images — go when it returns. What `f` keeps,
/// it retains, as AppKit does with what it is given.
pub(crate) fn with_pool<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: a push and its pop on the same thread, around `f`.
    let pool = unsafe { objc_autoreleasePoolPush() };
    let out = f();
    // SAFETY: the pool pushed above, popped once.
    unsafe { objc_autoreleasePoolPop(pool) };
    out
}
