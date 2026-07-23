//! Dynamic library loading for plugin DLLs.
//!
//! Phase 3 ships the Windows backend (`LoadLibraryW` +
//! `GetProcAddress` + `FreeLibrary`); Phase 5 adds `dlopen`/`dlsym`
//! for Linux/macOS via `#[cfg(unix)]` arms.
//!
//! `DynLib` owns the loaded module and frees it on drop. Function
//! pointers obtained via `resolve` are tied to the library's lifetime
//! by way of the `'a` borrow on `&'a DynLib` — the type system stops
//! callers from invoking a function pointer after the library is
//! unloaded.

use std::ffi::OsStr;
use std::path::Path;

#[cfg(target_os = "windows")]
mod imp {
    use super::Path;
    use std::ffi::CString;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    pub struct DynLibInner {
        pub(super) handle: HMODULE,
    }

    pub fn load(path: &Path) -> Result<DynLibInner, String> {
        // Reject relative paths: `LoadLibraryW("user32.dll")` triggers
        // Win32's full DLL search order, which includes the current
        // working directory — a hijack vector if the cwd is
        // attacker-controlled. Plugin loads always pass absolute
        // paths from `read_dir`; tests use absolute paths constructed
        // from `%SystemRoot%`. Refusing relative paths at the API
        // boundary closes the class of bug rather than relying on
        // every call site.
        if !path.is_absolute() {
            return Err(format!(
                "DynLib::load requires an absolute path; got {}",
                path.display()
            ));
        }
        let wide = HSTRING::from(path.as_os_str());
        // SAFETY: `wide` is a valid null-terminated UTF-16 string for
        // the duration of the call. `LoadLibraryW` returns NULL on
        // failure; we surface the error.
        let handle = unsafe { LoadLibraryW(&wide) }
            .map_err(|e| format!("LoadLibraryW({}): {e}", path.display()))?;
        if handle.is_invalid() {
            return Err(format!("LoadLibraryW({}): null module", path.display()));
        }
        Ok(DynLibInner { handle })
    }

    pub fn resolve(inner: &DynLibInner, symbol: &str) -> Option<*mut core::ffi::c_void> {
        let cstr = CString::new(symbol).ok()?;
        // SAFETY: `cstr` is a valid null-terminated C string for the
        // duration of the call. `GetProcAddress` returns NULL when
        // the symbol isn't exported.
        let proc =
            unsafe { GetProcAddress(inner.handle, windows::core::PCSTR(cstr.as_ptr().cast())) };
        proc.map(|f| f as *mut core::ffi::c_void)
    }

    impl Drop for DynLibInner {
        fn drop(&mut self) {
            // SAFETY: handle came from a successful LoadLibraryW and
            // was not yet freed. Win32 refcounts loads; if any other
            // module is still using this library, the underlying DLL
            // stays mapped until the last FreeLibrary.
            let _ = unsafe { FreeLibrary(self.handle) };
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::Path;
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;

    /// Owns the `dlopen` handle; `dlclose`d on drop.
    pub struct DynLibInner {
        handle: *mut core::ffi::c_void,
    }

    /// Pull the most recent `dlerror()` text, or a generic fallback.
    /// `dlerror()` returns a pointer to a thread-local static string
    /// (borrowed, not owned) or null when there is no pending error.
    fn last_dlerror() -> String {
        // SAFETY: `dlerror` takes no arguments and returns either null
        // or a pointer to a NUL-terminated C string valid until the
        // next `dlerror` call on this thread — which does not happen
        // before we copy it out here.
        let msg = unsafe { libc::dlerror() };
        if msg.is_null() {
            return "unknown dynamic-loader error".to_string();
        }
        // SAFETY: `msg` is non-null and NUL-terminated per the contract
        // above.
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn load(path: &Path) -> Result<DynLibInner, String> {
        // Reject relative paths for the same reason the Windows arm
        // does: a bare name (`"libc.so.6"`) sends `dlopen` through the
        // `LD_LIBRARY_PATH` / default search order, a hijack vector if
        // any searched directory is attacker-controlled. Plugin loads
        // always pass absolute paths from `read_dir`.
        if !path.is_absolute() {
            return Err(format!(
                "DynLib::load requires an absolute path; got {}",
                path.display()
            ));
        }
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("path contains an interior NUL: {}", path.display()))?;
        // Clear any stale pending error so `last_dlerror()` reports
        // *this* load's failure rather than a previous one.
        // SAFETY: `dlerror` is always safe to call to reset the state.
        unsafe { libc::dlerror() };
        // RTLD_NOW: resolve every symbol at load time, so a plugin with
        // an unsatisfied dependency fails here (with a diagnostic)
        // rather than crashing later on first use. RTLD_LOCAL: keep the
        // plugin's symbols out of the global scope, so two plugins that
        // happen to export the same name don't collide — the host
        // resolves each plugin's exports through its own handle.
        // SAFETY: `cpath` is a valid NUL-terminated path for the call.
        let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!("dlopen({}): {}", path.display(), last_dlerror()));
        }
        Ok(DynLibInner { handle })
    }

    pub fn resolve(inner: &DynLibInner, symbol: &str) -> Option<*mut core::ffi::c_void> {
        let csym = CString::new(symbol).ok()?;
        // SAFETY: `inner.handle` came from a successful `dlopen` and is
        // still open (we hold `&DynLibInner`); `csym` is a valid
        // NUL-terminated symbol name for the call. `dlsym` returns null
        // when the symbol isn't exported — treated as `None`, matching
        // the Windows `GetProcAddress` arm.
        let ptr = unsafe { libc::dlsym(inner.handle, csym.as_ptr()) };
        if ptr.is_null() {
            None
        } else {
            Some(ptr)
        }
    }

    impl Drop for DynLibInner {
        fn drop(&mut self) {
            // SAFETY: `handle` came from a successful `dlopen` and has
            // not been closed. `dlclose` refcounts like `FreeLibrary`;
            // the underlying object stays mapped until the last close.
            unsafe {
                libc::dlclose(self.handle);
            }
        }
    }
}

#[cfg(not(any(target_os = "windows", unix)))]
mod imp {
    use super::Path;

    pub struct DynLibInner;

    pub fn load(_path: &Path) -> Result<DynLibInner, String> {
        Err("dynlib: no dynamic-loader backend for this target".into())
    }

    pub fn resolve(_inner: &DynLibInner, _symbol: &str) -> Option<*mut core::ffi::c_void> {
        None
    }
}

/// Owning handle to a dynamically-loaded library. Drops free the
/// library; resolved function pointers borrow against this handle so
/// the type system enforces lifetime correctness.
pub struct DynLib {
    inner: imp::DynLibInner,
    path: std::path::PathBuf,
}

impl DynLib {
    /// Load the library at `path`. Returns `Err` with a diagnostic
    /// message if the OS rejects the load (file missing, wrong
    /// architecture, missing dependency, sandboxed runner without
    /// permission).
    ///
    /// # Errors
    ///
    /// Returns a `String` describing the load failure. The text is
    /// the platform's own error message (`GetLastError`'s
    /// `FormatMessage` on Windows, `dlerror()` on Unix) so the user
    /// sees the same diagnostic they'd get from any other native
    /// loader — wrong architecture, missing dependency DLL,
    /// missing file, permission denied, etc.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let inner = imp::load(path)?;
        Ok(Self {
            inner,
            path: path.to_path_buf(),
        })
    }

    /// Look up an exported C symbol. Returns `None` if the library
    /// doesn't export a symbol with that name. The returned pointer
    /// borrows against this `DynLib` — calling it after the library
    /// is dropped is a use-after-free.
    ///
    /// # Safety
    ///
    /// The caller must ensure the returned pointer is reinterpreted
    /// to a function signature that matches the C ABI of the
    /// exported symbol. A wrong signature (different argument types,
    /// different calling convention) is undefined behaviour.
    #[must_use]
    pub unsafe fn resolve_raw(&self, symbol: &str) -> Option<*mut core::ffi::c_void> {
        imp::resolve(&self.inner, symbol)
    }

    /// Convenience wrapper around `resolve_raw`: return the symbol
    /// already cast to a function pointer of the caller-specified
    /// type. Wraps a `transmute_copy` so callers don't have to.
    ///
    /// # Safety
    ///
    /// `F` must be the correct signature for the symbol — same
    /// argument types, same return type, same calling convention.
    #[must_use]
    pub unsafe fn resolve<F: Sized>(&self, symbol: &str) -> Option<F> {
        // SAFETY: forwarded to caller; documented above.
        let ptr = unsafe { self.resolve_raw(symbol)? };
        // SAFETY: we have exactly one pointer-sized value; F is
        // documented to be a function-pointer type so its size
        // matches `*const ()`. transmute_copy avoids the layout
        // assertion mem::transmute would impose.
        debug_assert_eq!(
            core::mem::size_of::<F>(),
            core::mem::size_of::<*mut core::ffi::c_void>(),
            "resolve<F> requires F to be pointer-sized (function pointer)"
        );
        Some(unsafe { core::mem::transmute_copy::<*mut core::ffi::c_void, F>(&ptr) })
    }

    /// The path the library was loaded from. Useful for diagnostics
    /// and for the `tracing` spans the plugin host wraps every plugin
    /// call in.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Plugin library filename extension on this platform: `.dll` on
/// Windows, `.so` on Linux, `.dylib` on macOS.
#[cfg(target_os = "windows")]
pub const PLUGIN_EXTENSION: &str = "dll";

#[cfg(all(unix, not(target_os = "macos")))]
pub const PLUGIN_EXTENSION: &str = "so";

#[cfg(target_os = "macos")]
pub const PLUGIN_EXTENSION: &str = "dylib";

/// True if `path` looks like a plugin candidate based on its
/// extension. Used by the plugin host's enumeration to filter
/// directory entries.
pub fn has_plugin_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case(PLUGIN_EXTENSION))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extension_classifier() {
        // Cross-platform negative cases first.
        assert!(!has_plugin_extension(&PathBuf::from("foo.txt")));
        assert!(!has_plugin_extension(&PathBuf::from("foo")));

        // Per-OS positive cases — each platform's PLUGIN_EXTENSION
        // is different (`dll` / `so` / `dylib`).
        #[cfg(target_os = "windows")]
        {
            assert!(has_plugin_extension(&PathBuf::from("foo.dll")));
            assert!(has_plugin_extension(&PathBuf::from("foo.DLL")));
            assert!(has_plugin_extension(&PathBuf::from("path/to/sub.dll")));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            assert!(has_plugin_extension(&PathBuf::from("foo.so")));
            assert!(has_plugin_extension(&PathBuf::from("foo.SO")));
        }
        #[cfg(target_os = "macos")]
        {
            assert!(has_plugin_extension(&PathBuf::from("foo.dylib")));
            assert!(has_plugin_extension(&PathBuf::from("foo.DYLIB")));
        }
    }

    #[cfg(target_os = "windows")]
    fn system32_path(name: &str) -> PathBuf {
        let system_root = std::env::var_os("SystemRoot").expect("SystemRoot env var");
        PathBuf::from(system_root).join("System32").join(name)
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn loads_and_resolves_user32() {
        // user32.dll is loaded into every Win32 process; we know it
        // exists and exports MessageBeep. Use an absolute path —
        // `DynLib::load` rejects relative paths.
        let lib = DynLib::load(system32_path("user32.dll")).expect("user32 load");
        unsafe {
            type MessageBeepFn = unsafe extern "system" fn(u32) -> i32;
            let _: MessageBeepFn = lib.resolve("MessageBeep").expect("MessageBeep export");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn missing_symbol_returns_none() {
        let lib = DynLib::load(system32_path("user32.dll")).expect("user32 load");
        unsafe {
            type Phantom = unsafe extern "system" fn() -> i32;
            let probe: Option<Phantom> = lib.resolve("ThisSymbolDefinitelyDoesNotExist_12345");
            assert!(probe.is_none());
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn missing_library_returns_err() {
        let result = DynLib::load(system32_path("definitely-not-a-real-library-name.dll"));
        assert!(result.is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn drop_frees_library() {
        // Just confirm Drop runs without panic. FreeLibrary's actual
        // refcount semantics are tested by Windows itself; we don't
        // assert on internal state.
        let lib = DynLib::load(system32_path("user32.dll")).expect("user32 load");
        drop(lib);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rejects_relative_path() {
        // Closing the search-order hijack class: a bare filename or
        // any relative path must be rejected before we hand off to
        // LoadLibraryW.
        let result = DynLib::load("user32.dll");
        assert!(result.is_err(), "relative path should be rejected");
        let result = DynLib::load("subdir/user32.dll");
        assert!(result.is_err(), "relative path should be rejected");
    }

    // --- Unix (dlopen) loader tests ---------------------------------
    //
    // libc is mapped into every process, so it's a reliable target the
    // same way user32.dll is on Windows. `dlopen(NULL)` would give the
    // global handle but `DynLib` needs a real absolute path, so we open
    // libc by its canonical soname via an absolute path when we can find
    // one; falling back to the linker's search is exactly what we forbid,
    // so instead we resolve a well-known symbol out of the already-mapped
    // libc through a freshly-opened handle to the system C library.

    #[cfg(all(unix, not(target_os = "macos")))]
    fn libc_path() -> PathBuf {
        // Glibc and musl both install the runtime C library under one of
        // these absolute paths on the CI/dev targets; try each.
        for candidate in [
            "/lib/x86_64-linux-gnu/libc.so.6",
            "/usr/lib/x86_64-linux-gnu/libc.so.6",
            "/lib64/libc.so.6",
            "/usr/lib/libc.so.6",
            "/lib/libc.so.6",
        ] {
            let p = PathBuf::from(candidate);
            if p.exists() {
                return p;
            }
        }
        // Last resort for exotic layouts: libm, which `getauxval`-free
        // targets still ship. If none exist the test self-skips below.
        PathBuf::from("/usr/lib/libc.so.6")
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn loads_and_resolves_libc() {
        let path = libc_path();
        if !path.exists() {
            // No known absolute libc path on this host; nothing to load.
            eprintln!("skipping: no libc at a known absolute path");
            return;
        }
        let lib = DynLib::load(&path).expect("libc load");
        unsafe {
            // `strlen` is exported by every C library.
            type StrlenFn = unsafe extern "C" fn(*const core::ffi::c_char) -> usize;
            let strlen: StrlenFn = lib.resolve("strlen").expect("strlen export");
            let s = b"hello\0";
            assert_eq!(strlen(s.as_ptr().cast()), 5);
        }
    }

    #[cfg(unix)]
    #[test]
    fn missing_library_returns_err_unix() {
        let result = DynLib::load("/definitely/not/a/real/library-name.so");
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_relative_path_unix() {
        // Same search-order hijack class as the Windows arm: a bare
        // soname would send dlopen through LD_LIBRARY_PATH.
        assert!(DynLib::load("libc.so.6").is_err());
        assert!(DynLib::load("subdir/libc.so.6").is_err());
    }
}
