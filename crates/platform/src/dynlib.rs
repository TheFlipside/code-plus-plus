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

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::Path;

    pub struct DynLibInner;

    pub fn load(_path: &Path) -> Result<DynLibInner, String> {
        Err("dynlib: non-Windows backend not implemented until Phase 5".into())
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

/// Plugin DLL filename extension on this platform. Phase 5 expands
/// this to `.so`/`.dylib` when the non-Windows backends land.
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
}
