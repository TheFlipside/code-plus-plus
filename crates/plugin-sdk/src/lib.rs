//! Shared FFI helpers for Code++ plugin cdylibs.
//!
//! Every in-tree plugin (`example-hello`, `cppmimetools`,
//! `cppconverter`, `cppexport`) used to duplicate the same Win32 +
//! Scintilla scaffolding: the `SyncCell<T>` wrapper for static
//! `FuncItem` arrays, the three `AtomicPtr<c_void>` slots for
//! `NppData` handles, the `extern "system" SendMessageW` link, the
//! NPPM/SCI message constants, the `menu_label` const-fn, and the
//! selection / status-bar round-trip helpers. This crate
//! consolidates them so each plugin's `imp.rs` can focus on the
//! transform body and menu-item layout.
//!
//! ## Linkage
//!
//! `codepp-plugin-sdk` is a plain `rlib`. Each plugin cdylib that
//! depends on it links a **private copy** of the SDK's `static`
//! atomics into its own binary — Rust's standard rlib semantics.
//! Two plugins loaded into the same host process therefore see
//! independent `NPP_HANDLE` / `SCINTILLA_*` slots; each plugin's
//! `setInfo` writes to its own copy.
//!
//! ## Cross-platform transport
//!
//! On Windows the plugin's `SendMessage(scintillaHandle, SCI_*, …)` is
//! routed by the OS message pump for free — the handle *is* the
//! Scintilla window — so `SendMessageW` is a direct `#[link(name =
//! "user32")]` import. On Linux/macOS a plugin `.so`/`.dylib` has no
//! Scintilla linked and there is no OS pump, so `SendMessageW` forwards
//! to a **host-installed callback** ([`HOST_DISPATCH`]) instead. The
//! host resolves [`codepp_plugin_set_dispatch`] in the loaded library
//! right after `dlopen` and hands the plugin a routing function that
//! sends `NPPM_*` to the host dispatcher and `SCI_*` to the Scintilla
//! widget. Every SDK helper (and every plugin) goes through the one
//! `SendMessageW` alias, so the transport swap is invisible above it.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};

// ---- Re-exports of the host's public FFI types ------------------
//
// Plugins build their `FuncItem` arrays and dispatch on
// `SCNotification` via these types. Re-exporting from the SDK means
// each plugin only needs `codepp-plugin-sdk` as a dep, not both
// `codepp-plugin-sdk` and `codepp-plugin-host`.

// `Hwnd` and `HostDispatchFn` are re-exported (not redefined) so this
// ABI-critical pair can never silently drift from the host's copy.
pub use codepp_plugin_host::ffi::{
    FuncItem, HostDispatchFn, Hwnd, NppData, SCNotification, MENU_TITLE_LENGTH,
};

// ---- SendMessageW transport -------------------------------------
//
// On Windows this is the Win32 `#[link(name = "user32")]` import; the
// attribute propagates to dependent cdylibs at link time, so each
// plugin gets `user32.dll` resolved without re-declaring the link. On
// non-Windows it forwards to the host callback installed via
// [`codepp_plugin_set_dispatch`]. Either way it is `pub` so plugins can
// send one-off messages the helpers below don't cover, and the two
// definitions share one signature so call sites are identical.

#[cfg(target_os = "windows")]
#[link(name = "user32")]
extern "system" {
    /// Win32 `SendMessageW`. Synchronous round-trip into the target
    /// window's `wnd_proc`; returns whatever the message handler produced.
    pub fn SendMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
}

/// The host callback installed by [`codepp_plugin_set_dispatch`]. Null
/// until the host runs the handshake (i.e. before the plugin's
/// `setInfo`), the same window in which the handle atomics are also
/// null — so a message sent that early is a no-op either way.
#[cfg(not(target_os = "windows"))]
static HOST_DISPATCH: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

/// Install the host's message-routing callback into this plugin.
///
/// The host resolves this symbol in each freshly-loaded plugin (via
/// `dlsym`) and calls it once, before `setInfo`, passing the routing
/// function. It replaces the Win32 OS message pump the Windows build
/// gets for free. A null argument clears the callback.
///
/// # Safety
///
/// The host must pass a function pointer valid for the plugin's entire
/// loaded lifetime, matching [`HostDispatchFn`]'s ABI.
#[cfg(not(target_os = "windows"))]
#[no_mangle]
pub extern "C" fn codepp_plugin_set_dispatch(dispatch: Option<HostDispatchFn>) {
    let ptr = dispatch.map_or(core::ptr::null_mut(), |f| f as *mut c_void);
    HOST_DISPATCH.store(ptr, Ordering::Release);
}

/// Non-Windows `SendMessageW`: forward to the host callback installed
/// via [`codepp_plugin_set_dispatch`]. Returns 0 when no callback is
/// installed yet — mirroring Win32 `SendMessage` to a null/invalid
/// window, which also returns 0 without dereferencing.
///
/// # Safety
///
/// Same contract as Win32 `SendMessageW`: `wparam`/`lparam` must be
/// valid for whatever the message documents (e.g. a valid out-pointer
/// for `NPPM_GETCURRENTSCINTILLA`), and the host callback must be a live
/// [`HostDispatchFn`].
#[cfg(not(target_os = "windows"))]
#[allow(non_snake_case)]
pub unsafe fn SendMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize {
    let ptr = HOST_DISPATCH.load(Ordering::Acquire);
    if ptr.is_null() {
        return 0;
    }
    // SAFETY: `ptr` was stored from a valid `HostDispatchFn` by
    // `codepp_plugin_set_dispatch`; transmuting it back to that fn type
    // and calling it is sound. The message-argument contract is the
    // caller's responsibility, exactly as for the Win32 import.
    let dispatch: HostDispatchFn = unsafe { core::mem::transmute(ptr) };
    unsafe { dispatch(hwnd, msg, wparam, lparam) }
}

// ---- SyncCell ---------------------------------------------------
//
// Wrapper providing `Sync` for `UnsafeCell<T>`. The host writes
// `cmd_id` into our static FuncItem array at load time — the
// inherent "shared memory mutated by foreign code" pattern
// `UnsafeCell` exists for. The plugin itself never reads back the
// mutated field; we just hand the host a pointer it owns.
//
// We deliberately do **not** bound `T: Send`: `FuncItem` carries a
// `*mut ShortcutKey` raw pointer that prevents auto-derivation of
// `Send`, but the host's access is single-threaded (the UI thread
// only) and the only field it mutates is `cmd_id`, which is plain
// `i32`. The bound would refuse a perfectly safe pattern.

/// `Sync`-friendly cell for static `FuncItem` arrays.
#[repr(transparent)]
pub struct SyncCell<T>(UnsafeCell<T>);

// SAFETY: see the module-level comment above. The host's
// single-threaded mutation of `cmd_id` is the only write; plugin-
// side reads of the array don't observe `cmd_id`.
unsafe impl<T> Sync for SyncCell<T> {}

impl<T> SyncCell<T> {
    /// Construct a fresh `SyncCell` wrapping `val`. `const` so
    /// plugins can build a static `FuncItem` array at compile time.
    pub const fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }

    /// Returns a raw mutable pointer to the inner value. The host's
    /// loader writes into the `FuncItem` array via this pointer.
    pub fn get(&self) -> *mut T {
        self.0.get()
    }
}

// ---- menu_label() ----------------------------------------------

/// Build a fixed-size NUL-padded UTF-16 menu label from an ASCII
/// byte string. `const fn` so the `FuncItem` array initialiser is
/// a compile-time constant. Truncates at `MENU_TITLE_LENGTH - 1`
/// to leave room for the implicit NUL terminator that the host's
/// `TCM_INSERTITEMW` round-trip relies on.
///
/// **ASCII-only.** Non-ASCII bytes can't be `as`-cast to `u16`
/// losslessly (the upper byte of a multibyte UTF-8 sequence isn't
/// a valid UTF-16 code unit), so we assert at compile time —
/// because this is a `const fn` invoked during static
/// initialisation, an assertion failure becomes a build error
/// rather than a runtime panic. Plugin authors who need
/// non-ASCII labels would need a separate non-const helper that
/// does proper UTF-8 → UTF-16 conversion; we don't ship one yet
/// because every in-tree plugin's labels are ASCII.
///
/// # Panics
///
/// Compile-time panic via `const_panic` when any input byte is
/// `>= 128` (non-ASCII). Build-time only; never reaches runtime
/// because `menu_label` is invoked from `const` contexts at the
/// plugin authoring site.
#[must_use]
pub const fn menu_label(bytes: &[u8]) -> [u16; MENU_TITLE_LENGTH] {
    let mut buf = [0u16; MENU_TITLE_LENGTH];
    let mut i = 0;
    while i < bytes.len() && i < MENU_TITLE_LENGTH - 1 {
        assert!(
            bytes[i] < 128,
            "menu_label requires ASCII bytes; use a UTF-8 helper for non-ASCII labels",
        );
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

// ---- NPPM/SCI constants -----------------------------------------
//
// The minimal set every selection-mutating plugin needs. Plugins
// with more exotic message use (cppexport's per-style queries,
// example-hello's SCI_INSERTTEXT) declare their own constants
// locally to keep the SDK's surface from sprawling.

/// `WM_USER + 1000` — base for every NPPM_* message id.
pub const NPPMSG: u32 = 0x0400 + 1000;

/// `NPPM_GETCURRENTSCINTILLA(0, &mut int)` — write the active view
/// index (0 = main, 1 = secondary) through the lparam pointer.
/// Used by [`active_scintilla`] to discover which Scintilla HWND
/// is currently bound.
pub const NPPM_GETCURRENTSCINTILLA: u32 = NPPMSG + 4;

/// `NPPM_SETSTATUSBAR(section, *wchar)` — overlay a UTF-16 string
/// onto the host's status bar. Used by [`set_status`].
pub const NPPM_SETSTATUSBAR: u32 = NPPMSG + 24;

/// Status-bar slot 0 (the doc-type field). [`set_status`] writes
/// here.
pub const STATUSBAR_DOC_TYPE: usize = 0;

/// `SCI_GETSELTEXT` — copy the selection (and, in Scintilla 5,
/// return the byte length without the trailing NUL).
pub const SCI_GETSELTEXT: u32 = 2161;

/// `SCI_GETSELECTIONSTART` — document position of the selection
/// anchor. Used by [`replace_selection`] to set the target range.
pub const SCI_GETSELECTIONSTART: u32 = 2143;

/// `SCI_GETSELECTIONEND` — document position of the selection
/// caret. Used by [`replace_selection`] to set the target range.
pub const SCI_GETSELECTIONEND: u32 = 2145;

/// `SCI_SETTARGETRANGE(start, end)` — set the target range for
/// subsequent `SCI_REPLACETARGET` / `SCI_SEARCHIN*` calls.
pub const SCI_SETTARGETRANGE: u32 = 2686;

/// `SCI_REPLACETARGET(length, *char)` — explicit-length replace
/// over the target range. **Binary-safe**, unlike `SCI_REPLACESEL`
/// (which reads its `lparam` as a NUL-terminated C string and
/// silently truncates at the first interior `\0`).
pub const SCI_REPLACETARGET: u32 = 2194;

// ---- NppData handle storage -------------------------------------

static NPP_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static SCINTILLA_MAIN: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static SCINTILLA_SECONDARY: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

/// Snapshot the three handles from the host's `setInfo` call into
/// the SDK's per-plugin static atomics. The plugin's exported
/// `setInfo` should be a one-line forward to this:
///
/// ```ignore
/// #[no_mangle]
/// pub extern "C" fn setInfo(data: NppData) {
///     codepp_plugin_sdk::store_handles(data);
/// }
/// ```
///
/// Atomics so a hypothetical concurrent menu callback (today
/// always single-threaded UI dispatch, but defense in depth)
/// reads consistent values without locking. `Release` write
/// pairs with the `Acquire` reads inside [`active_scintilla`] /
/// [`set_status`].
pub fn store_handles(data: NppData) {
    NPP_HANDLE.store(data.npp_handle, Ordering::Release);
    SCINTILLA_MAIN.store(data.scintilla_main_handle, Ordering::Release);
    SCINTILLA_SECONDARY.store(data.scintilla_second_handle, Ordering::Release);
}

/// Read the host's main-window HWND. Returns null until
/// [`store_handles`] has run (i.e. before the plugin's `setInfo`
/// fires); callers should treat null as "give up silently".
pub fn npp_handle() -> Hwnd {
    NPP_HANDLE.load(Ordering::Acquire)
}

// The selection / status helpers below take raw `Hwnd` pointers
// without an `unsafe fn` marker, which clippy's
// `not_unsafe_ptr_arg_deref` lint flags by default. The contract
// that makes these safe at the SDK boundary:
//
//   * Each helper null-checks the input HWND and returns
//     empty / no-op on null.
//   * For non-null but dangling / unrecognised handles, the
//     transport fails soft on both platforms: Win32's
//     `SendMessageW` returns 0 without dereferencing (documented
//     behaviour), and the non-Windows host routing identity-checks
//     the handle (npp sentinel vs. its own Scintilla widget) and
//     returns 0 for anything else rather than dereferencing it. So
//     plugins can't corrupt memory through these APIs on either
//     backend; the worst case is "nothing happens".
//   * Plugins always source HWNDs through `active_scintilla()`
//     or `npp_handle()`, both of which wrap atomics that the
//     SDK initialised from `setInfo`.
//
// Marking the helpers `unsafe fn` would force every plugin
// menu callback into an `unsafe { sdk::get_selection_bytes(...) }`
// wrapper without buying any safety the contract above doesn't
// already provide. The `#[allow]` keeps the API ergonomic.

/// Resolve the active Scintilla view's HWND.
///
/// Asks the host (`NPPM_GETCURRENTSCINTILLA` writes 0 = main / 1 =
/// secondary into the out-pointer) and looks up the matching handle
/// stored by [`store_handles`]. Returns null if `setInfo` hasn't
/// run yet — menu callbacks treat null as "give up silently",
/// which matches Notepad++'s plugin convention.
pub fn active_scintilla() -> Hwnd {
    let npp = NPP_HANDLE.load(Ordering::Acquire);
    if npp.is_null() {
        return core::ptr::null_mut();
    }
    let mut which: i32 = 0;
    // SAFETY: `&mut which` is a valid `int*` for the SendMessage
    // call. NPPM_GETCURRENTSCINTILLA is documented to write through
    // it (the host's dispatcher implements that contract).
    unsafe {
        SendMessageW(npp, NPPM_GETCURRENTSCINTILLA, 0, &raw mut which as isize);
    }
    // Clamp to the documented range. A buggy or hostile host
    // dispatcher could write a garbage value through `which`;
    // treating non-{0,1} as "no view available" is safer than
    // silently routing to the secondary slot, which is null in
    // single-view mode and would mask the misdispatch.
    match which {
        0 => SCINTILLA_MAIN.load(Ordering::Acquire),
        1 => SCINTILLA_SECONDARY.load(Ordering::Acquire),
        _ => core::ptr::null_mut(),
    }
}

// ---- Selection round-trip ---------------------------------------

/// Read the active selection as raw bytes via `SCI_GETSELTEXT`.
///
/// Two-phase: first call with null `text` gets the length; second
/// call with a sized buffer fills it. Scintilla 5 returns the byte
/// count of the selection (without the NUL); we allocate `len + 1`
/// so Scintilla can write its own terminator into the trailing
/// byte, then truncate it off before returning.
///
/// Hardened with `usize::try_from` + `checked_add` so a
/// pathological `isize::MAX` return on a 32-bit target can't
/// underflow into an undersized buffer — defense in depth, since
/// Code++ targets `x86_64` today and Scintilla returns sane values
/// for any real document.
///
/// **No TOCTOU between length and fill.** The two-phase
/// `SCI_GETSELTEXT` call structure superficially looks racy
/// (length-then-fill could read different selection sizes if
/// something edited the buffer in between), but Scintilla and
/// the SDK both run on the host's single UI thread *and* this
/// helper is invoked only from menu callbacks the host
/// dispatches synchronously inside its own `wnd_proc`. No other
/// code path can mutate the selection mid-call. If a future
/// path ever calls this off the UI thread or across an `await`
/// boundary, the assumption breaks and the fill could overflow
/// the buffer — flag it then.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn get_selection_bytes(sci: Hwnd) -> Vec<u8> {
    if sci.is_null() {
        return Vec::new();
    }
    // SAFETY: passing wparam=0, lparam=0 to SCI_GETSELTEXT asks
    // for the length only and writes nothing through any pointer.
    let len = unsafe { SendMessageW(sci, SCI_GETSELTEXT, 0, 0) };
    if len <= 0 {
        return Vec::new();
    }
    let Ok(len_us) = usize::try_from(len) else {
        return Vec::new();
    };
    let Some(alloc) = len_us.checked_add(1) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; alloc];
    // SAFETY: `buf.as_mut_ptr()` is valid for `alloc` bytes
    // (= `len_us + 1`). Scintilla writes the selection content
    // followed by a NUL terminator into the buffer, totalling
    // `len_us + 1` bytes — see the no-TOCTOU note in the
    // function-level doc-comment above for why the length read
    // by the prior call is still authoritative when this fill
    // call runs.
    unsafe {
        SendMessageW(sci, SCI_GETSELTEXT, 0, buf.as_mut_ptr() as isize);
    }
    buf.truncate(len_us);
    buf
}

/// Replace the active selection with `bytes`.
///
/// Uses the `SCI_SETTARGETRANGE` then `SCI_REPLACETARGET` pair so
/// the replacement is binary-safe: `SCI_REPLACETARGET` takes an
/// explicit length, whereas `SCI_REPLACESEL` reads its `lparam` as
/// a NUL-terminated C string and would silently truncate at the
/// first interior `\0`. Matters for any decode path (HEX, Base64,
/// Quoted-Printable) that can produce embedded NULs.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn replace_selection(sci: Hwnd, bytes: &[u8]) {
    if sci.is_null() {
        return;
    }
    // SAFETY: pure queries, no pointer arguments.
    let (start, end) = unsafe {
        (
            SendMessageW(sci, SCI_GETSELECTIONSTART, 0, 0),
            SendMessageW(sci, SCI_GETSELECTIONEND, 0, 0),
        )
    };
    // SAFETY: SCI_SETTARGETRANGE takes wparam=start, lparam=end as
    // document positions. No pointer arguments. The
    // `start as usize` cast is intentional — Scintilla document
    // positions are non-negative isize values; the lint
    // suppression is local.
    #[allow(clippy::cast_sign_loss)]
    let start_usize = start as usize;
    unsafe {
        SendMessageW(sci, SCI_SETTARGETRANGE, start_usize, end);
    }
    // SAFETY: SCI_REPLACETARGET reads `wparam` bytes from `lparam`.
    // `bytes.as_ptr()` is valid for `bytes.len()` bytes for the
    // duration of the call; Scintilla doesn't retain the pointer.
    unsafe {
        SendMessageW(sci, SCI_REPLACETARGET, bytes.len(), bytes.as_ptr() as isize);
    }
}

// ---- Status bar -------------------------------------------------

/// Set the host status bar's "doc-type" pane (slot 0) to the given
/// text. Plugins drive this for transient feedback ("no selection",
/// "wrote /path/foo.html", etc.); the host's own status update
/// repaints the standard fields on the next chrome refresh.
///
/// No-op if `setInfo` hasn't run yet (`NPP_HANDLE` is null).
pub fn set_status(text: &str) {
    let npp = NPP_HANDLE.load(Ordering::Acquire);
    if npp.is_null() {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().chain(core::iter::once(0)).collect();
    // SAFETY: `wide.as_ptr()` is a valid NUL-terminated UTF-16
    // buffer for the duration of the call. NPPM_SETSTATUSBAR's
    // `lparam` is documented to take a `wchar_t*`.
    unsafe {
        SendMessageW(
            npp,
            NPPM_SETSTATUSBAR,
            STATUSBAR_DOC_TYPE,
            wide.as_ptr() as isize,
        );
    }
}
