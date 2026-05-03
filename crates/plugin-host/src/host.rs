//! Plugin discovery, loading, and lifecycle.
//!
//! Phase 3 milestone 2 ships:
//!   - directory enumeration (record paths, no LoadLibrary yet — DESIGN.md
//!     §6.4 mandates "loading is deferred"),
//!   - lazy load on first user touch (resolve six entry points, call
//!     `setInfo`, call `getFuncsArray`),
//!   - plugin lifecycle state machine (Pending → Loaded / Failed →
//!     ShuttingDown).
//!
//! NPPM_*/NPPN_* dispatching, menu integration, and the actual
//! example-hello DLL land in subsequent milestones.

#![cfg(target_os = "windows")]

use std::path::{Path, PathBuf};

use codepp_platform::{has_plugin_extension, DynLib};

use crate::ffi::{
    BeNotifiedFn, FuncItem, GetFuncsArrayFn, GetNameFn, IsUnicodeFn, MessageProcFn, NppData,
    SetInfoFn,
};

/// One discovered plugin candidate, by path. Holds whichever lifecycle
/// state the plugin is currently in.
pub struct PluginInfo {
    /// Filesystem path the plugin was discovered at.
    pub path: PathBuf,
    /// Display name. `None` until the plugin is loaded and `getName`
    /// returns a value; falls back to the file stem in UI.
    pub name: Option<String>,
    /// Lifecycle state.
    state: PluginState,
}

impl PluginInfo {
    /// True if the plugin has been loaded (LoadLibrary'd, six entry
    /// points resolved, getFuncsArray called).
    pub fn is_loaded(&self) -> bool {
        matches!(self.state, PluginState::Loaded(_))
    }

    /// True if a load attempt failed. `reason` carries the diagnostic.
    pub fn failed_reason(&self) -> Option<&str> {
        if let PluginState::Failed(r) = &self.state {
            Some(r.as_str())
        } else {
            None
        }
    }

    /// Best-effort display label for the UI. Loaded plugins use their
    /// `getName` return value; unloaded plugins fall back to the
    /// filename stem ("convert-tabs-spaces" for "convert-tabs-spaces.dll").
    pub fn display_label(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        self.path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<unnamed plugin>".to_string())
    }

    /// Functions the plugin contributed to the Plugins menu, if it has
    /// been loaded.
    pub fn func_items(&self) -> Option<&[FuncItem]> {
        if let PluginState::Loaded(p) = &self.state {
            Some(&p.funcs)
        } else {
            None
        }
    }

    /// `beNotified` entry point if loaded. The dispatcher (next
    /// milestone) iterates plugins and calls this with each
    /// SCNotification it wants to deliver.
    pub fn be_notified_fn(&self) -> Option<BeNotifiedFn> {
        if let PluginState::Loaded(p) = &self.state {
            Some(p.be_notified)
        } else {
            None
        }
    }

    /// `messageProc` entry point if loaded. Used by the host when it
    /// wants to send a custom message to a specific plugin (NPPM
    /// inter-plugin messaging is a Phase 4 concern; the accessor lives
    /// here so the dispatcher can call it from Phase 3 onward).
    pub fn message_proc_fn(&self) -> Option<MessageProcFn> {
        if let PluginState::Loaded(p) = &self.state {
            Some(p.message_proc)
        } else {
            None
        }
    }

    /// `isUnicode` entry point if loaded. Phase 3 always loads
    /// Unicode plugins (ANSI conversion is out of scope), but the
    /// accessor lets the dispatcher refuse to forward wide-char
    /// payloads to a plugin that returned FALSE.
    pub fn is_unicode_fn(&self) -> Option<IsUnicodeFn> {
        if let PluginState::Loaded(p) = &self.state {
            Some(p.is_unicode)
        } else {
            None
        }
    }

    /// `setInfo` entry point if loaded. Exposed for diagnostic
    /// re-injection of `NppData` (Phase 3 calls it once at load time;
    /// later phases may re-call after split-view changes).
    pub fn set_info_fn(&self) -> Option<SetInfoFn> {
        if let PluginState::Loaded(p) = &self.state {
            Some(p.set_info)
        } else {
            None
        }
    }

    /// `getName` entry point if loaded. The cached `name` field is
    /// the typical access path; this accessor is for plugins that
    /// rename themselves at runtime.
    pub fn get_name_fn(&self) -> Option<GetNameFn> {
        if let PluginState::Loaded(p) = &self.state {
            Some(p.get_name)
        } else {
            None
        }
    }

    /// `getFuncsArray` entry point if loaded. Re-callable for plugins
    /// that mutate their menu set after init (rare; mostly Phase 4+).
    pub fn get_funcs_array_fn(&self) -> Option<GetFuncsArrayFn> {
        if let PluginState::Loaded(p) = &self.state {
            Some(p.get_funcs_array)
        } else {
            None
        }
    }
}

enum PluginState {
    Pending,
    Loaded(LoadedPlugin),
    Failed(String),
}

/// State of a successfully loaded plugin. Holds the DynLib (drops
/// FreeLibrary at shutdown), the resolved entry-point function
/// pointers, and the cached FuncItem array.
struct LoadedPlugin {
    /// The DynLib's job is to keep the underlying DLL mapped: when
    /// `LoadedPlugin` drops, `lib` drops, which calls `FreeLibrary`
    /// and unloads the plugin. Clippy does not count `Drop` as a
    /// field-read, so the field appears unread to dead-code analysis;
    /// the allow attribute documents the intentional ownership.
    #[allow(dead_code)]
    lib: DynLib,
    set_info: SetInfoFn,
    get_name: GetNameFn,
    get_funcs_array: GetFuncsArrayFn,
    be_notified: BeNotifiedFn,
    message_proc: MessageProcFn,
    is_unicode: IsUnicodeFn,
    /// Snapshot of the FuncItem array the plugin returned. Each
    /// FuncItem is `Copy` (no heap pointers we own — `p_sh_key` is
    /// owned by the plugin), so cloning is safe.
    funcs: Vec<FuncItem>,
    /// Plugin's getName return value, decoded to UTF-8.
    name: String,
}

/// Top-level plugin registry. Owned by the shell; UI crates poke it
/// through `Shell` to enumerate, load, dispatch.
#[derive(Default)]
pub struct PluginHost {
    plugins: Vec<PluginInfo>,
}

impl PluginHost {
    /// Construct an empty registry. Call [`PluginHost::discover`] to
    /// populate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enumerate plugin candidates in `dir`. Each `*.dll` becomes a
    /// `PluginInfo` in the `Pending` state — the file is **not** yet
    /// LoadLibrary'd. Returns the count discovered.
    ///
    /// A non-existent directory is not an error; it's the first-run
    /// case. The scan walks **two** subdirectory levels deep so all
    /// three of these layouts are picked up:
    ///
    ///   plugins/<name>.dll                       (depth 0)
    ///   plugins/<name>/<name>.dll                (depth 1, the
    ///                                             Notepad++ default)
    ///   plugins/<name>/<archdir>/<name>.dll      (depth 2, the
    ///                                             NppExec /
    ///                                             ComparePlugin
    ///                                             64-bit layout)
    ///
    /// Symlinks: `is_dir()`/`is_file()` follow symlinks, so a
    /// directory symlink in the plugins folder is enumerated. On
    /// Windows symlink creation requires SeCreateSymbolicLinkPrivilege
    /// by default, so this is low-severity. Phase 5 (Linux/macOS,
    /// where symlink creation is unprivileged) will need to validate
    /// resolved paths stay within `dir` or use `O_NOFOLLOW`.
    pub fn discover(&mut self, dir: &Path) -> std::io::Result<usize> {
        if !dir.exists() {
            return Ok(0);
        }
        let mut found = 0usize;
        self.discover_walk(dir, 0, 2, &mut found)?;
        Ok(found)
    }

    fn discover_walk(
        &mut self,
        dir: &Path,
        depth: u32,
        max_depth: u32,
        found: &mut usize,
    ) -> std::io::Result<()> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && has_plugin_extension(&path) {
                self.plugins.push(PluginInfo {
                    path,
                    name: None,
                    state: PluginState::Pending,
                });
                *found += 1;
            } else if path.is_dir() && depth < max_depth {
                let _ = self.discover_walk(&path, depth + 1, max_depth, found);
            }
        }
        Ok(())
    }

    /// Total number of plugins (any state).
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// True if no plugins are registered.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Iterate all known plugins regardless of lifecycle state.
    pub fn iter(&self) -> impl Iterator<Item = &PluginInfo> {
        self.plugins.iter()
    }

    /// Load the plugin at index `idx` if it is currently `Pending`.
    /// Calls `setInfo(npp_data)` and `getFuncsArray` as part of the
    /// load — same order Notepad++ uses, so existing plugins observe
    /// the same lifecycle.
    ///
    /// On error the plugin moves to `Failed(reason)` and `Err(reason)`
    /// is returned. The plugin entry stays in the registry for
    /// diagnostic display; the host doesn't retry automatically.
    pub fn load(&mut self, idx: usize, npp_data: NppData) -> Result<(), String> {
        let Some(plugin) = self.plugins.get_mut(idx) else {
            return Err(format!("plugin index {idx} out of range"));
        };
        if plugin.is_loaded() {
            return Ok(());
        }

        let path = plugin.path.clone();
        let _span = tracing::info_span!("plugin_load", path = %path.display()).entered();

        let result = load_inner(&path, npp_data);
        match result {
            Ok(loaded) => {
                plugin.name = Some(loaded.name.clone());
                plugin.state = PluginState::Loaded(loaded);
                Ok(())
            }
            Err(e) => {
                plugin.state = PluginState::Failed(e.clone());
                Err(e)
            }
        }
    }
}

/// Resolve the six entry points and run the initial setInfo +
/// getFuncsArray dance. Returns a fully-populated `LoadedPlugin` on
/// success.
fn load_inner(path: &Path, npp_data: NppData) -> Result<LoadedPlugin, String> {
    let lib = DynLib::load(path)?;

    // SAFETY: each resolve call casts the GetProcAddress result to
    // the function pointer type declared in `ffi`. Those types match
    // the C ABI declared in PluginInterface.h. A plugin that doesn't
    // export one of these is rejected (Err below).
    let (set_info, get_name, get_funcs_array, be_notified, message_proc, is_unicode) = unsafe {
        let set_info: SetInfoFn = lib
            .resolve("setInfo")
            .ok_or("missing entry point: setInfo")?;
        let get_name: GetNameFn = lib
            .resolve("getName")
            .ok_or("missing entry point: getName")?;
        let get_funcs_array: GetFuncsArrayFn = lib
            .resolve("getFuncsArray")
            .ok_or("missing entry point: getFuncsArray")?;
        let be_notified: BeNotifiedFn = lib
            .resolve("beNotified")
            .ok_or("missing entry point: beNotified")?;
        let message_proc: MessageProcFn = lib
            .resolve("messageProc")
            .ok_or("missing entry point: messageProc")?;
        let is_unicode: IsUnicodeFn = lib
            .resolve("isUnicode")
            .ok_or("missing entry point: isUnicode")?;
        (
            set_info,
            get_name,
            get_funcs_array,
            be_notified,
            message_proc,
            is_unicode,
        )
    };

    // setInfo first — plugin stashes the host handles before we ask
    // it for menu items. Wrap each FFI call in `catch_unwind` so a
    // Rust-authored plugin that panics doesn't unwind across the C
    // ABI (that's UB; DESIGN.md §6.5). C++ plugins that throw past
    // their own ABI are out of scope — broken in Notepad++ too.
    use std::panic::{catch_unwind, AssertUnwindSafe};
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: set_info has the C ABI declared in
        // PluginInterface.h; npp_data is a valid #[repr(C)] NppData
        // by construction.
        unsafe { set_info(npp_data) }
    }))
    .map_err(|_| "plugin panicked in setInfo".to_string())?;

    // getName — wide-char string the host displays in the menu.
    let name = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: pointer is documented to remain valid for the
        // plugin's lifetime (per PluginInterface.h). We copy the
        // bytes into an owned String immediately so we don't hold
        // the pointer past this call.
        unsafe {
            let p = get_name();
            if p.is_null() {
                "<unnamed>".to_string()
            } else {
                wide_to_string(p)
            }
        }
    }))
    .map_err(|_| "plugin panicked in getName".to_string())?;

    // getFuncsArray — plugin returns a pointer to its menu items.
    let mut count: i32 = 0;
    let raw = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: get_funcs_array signature declared in ffi; count
        // is a valid out-pointer.
        unsafe { get_funcs_array(&mut count as *mut i32) }
    }))
    .map_err(|_| "plugin panicked in getFuncsArray".to_string())?;
    if raw.is_null() || count <= 0 {
        // Allow plugins that contribute no menu items — they may
        // still be useful via beNotified-only lifecycles.
        let funcs = Vec::new();
        return Ok(LoadedPlugin {
            lib,
            set_info,
            get_name,
            get_funcs_array,
            be_notified,
            message_proc,
            is_unicode,
            funcs,
            name,
        });
    }

    // Copy the FuncItem array out of the plugin's address space into
    // our own Vec. FuncItem is `Copy` so each element is bitwise-
    // duplicated; the plugin retains ownership of the original
    // memory and any `p_sh_key` accelerator pointers it allocated.
    // SAFETY: raw is non-null and points to `count` valid FuncItem
    // values (per the plugin's contract). We read each element by
    // value.
    let funcs = unsafe {
        let count = count as usize;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            out.push(*raw.add(i));
        }
        out
    };

    Ok(LoadedPlugin {
        lib,
        set_info,
        get_name,
        get_funcs_array,
        be_notified,
        message_proc,
        is_unicode,
        funcs,
        name,
    })
}

/// Decode a null-terminated wide-char string (`*const u16`) into an
/// owned UTF-8 `String`. Bounded scan to 4096 chars to avoid running
/// off into arbitrary memory if the plugin returns an unterminated
/// pointer; truncation is preferable to a buffer over-read.
unsafe fn wide_to_string(mut p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut units = Vec::with_capacity(64);
    let max = 4096;
    let mut count = 0;
    // SAFETY: bounded by `max`; null-terminator stops the loop.
    unsafe {
        while count < max {
            let c = *p;
            if c == 0 {
                break;
            }
            units.push(c);
            p = p.add(1);
            count += 1;
        }
    }
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_missing_dir_is_zero() {
        let mut host = PluginHost::new();
        let n = host
            .discover(&PathBuf::from("definitely-not-a-real-plugin-dir-12345"))
            .unwrap();
        assert_eq!(n, 0);
        assert!(host.is_empty());
    }

    #[test]
    fn discover_empty_dir_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = PluginHost::new();
        let n = host.discover(dir.path()).unwrap();
        assert_eq!(n, 0);
        assert!(host.is_empty());
    }

    #[test]
    fn discover_skips_non_dlls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readme.txt"), "ignore me").unwrap();
        std::fs::write(dir.path().join("data.json"), "{}").unwrap();

        let mut host = PluginHost::new();
        host.discover(dir.path()).unwrap();
        assert!(host.is_empty());
    }

    #[test]
    fn discover_finds_top_level_dlls() {
        let dir = tempfile::tempdir().unwrap();
        // Create empty files with .dll extensions; we don't try to load
        // them in this test — discovery is filesystem-only.
        std::fs::write(dir.path().join("plugin-a.dll"), b"not a real dll").unwrap();
        std::fs::write(dir.path().join("plugin-b.dll"), b"also not real").unwrap();
        std::fs::write(dir.path().join("notes.md"), b"skip me").unwrap();

        let mut host = PluginHost::new();
        let n = host.discover(dir.path()).unwrap();
        assert_eq!(n, 2);
        let names: std::collections::HashSet<_> = host
            .iter()
            .map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains("plugin-a.dll"));
        assert!(names.contains("plugin-b.dll"));
    }

    #[test]
    fn discover_finds_subdir_dlls() {
        let dir = tempfile::tempdir().unwrap();
        let sub_a = dir.path().join("plugin-a");
        std::fs::create_dir(&sub_a).unwrap();
        std::fs::write(sub_a.join("plugin-a.dll"), b"x").unwrap();

        let sub_b = dir.path().join("plugin-b");
        std::fs::create_dir(&sub_b).unwrap();
        std::fs::write(sub_b.join("plugin-b.dll"), b"x").unwrap();

        let mut host = PluginHost::new();
        let n = host.discover(dir.path()).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn discover_finds_depth2_dlls() {
        // NppExec / ComparePlugin layout:
        //   plugins/<name>/<archdir>/<name>.dll
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("nppexec");
        let arch_dir = plugin_dir.join("nppexec64");
        std::fs::create_dir_all(&arch_dir).unwrap();
        std::fs::write(arch_dir.join("nppexec.dll"), b"x").unwrap();

        let mut host = PluginHost::new();
        let n = host.discover(dir.path()).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            host.iter().next().unwrap().path.file_name().unwrap(),
            "nppexec.dll"
        );
    }

    #[test]
    fn discover_does_not_recurse_past_depth2() {
        // Anything at depth 3+ is skipped — we don't want to walk
        // arbitrary trees.
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("too-deep.dll"), b"x").unwrap();

        let mut host = PluginHost::new();
        let n = host.discover(dir.path()).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn pending_plugin_falls_back_to_filename() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my-plugin.dll"), b"x").unwrap();
        let mut host = PluginHost::new();
        host.discover(dir.path()).unwrap();
        assert_eq!(host.iter().next().unwrap().display_label(), "my-plugin");
    }

    #[test]
    fn load_invalid_dll_marks_failed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("not-a-dll.dll"), b"this isn't a real dll").unwrap();
        let mut host = PluginHost::new();
        host.discover(dir.path()).unwrap();

        let npp_data = NppData {
            npp_handle: core::ptr::null_mut(),
            scintilla_main_handle: core::ptr::null_mut(),
            scintilla_second_handle: core::ptr::null_mut(),
        };
        let result = host.load(0, npp_data);
        assert!(result.is_err());
        let info = host.iter().next().unwrap();
        assert!(!info.is_loaded());
        assert!(info.failed_reason().is_some());
    }

    #[test]
    fn load_out_of_range_idx_errors() {
        let mut host = PluginHost::new();
        let npp_data = NppData {
            npp_handle: core::ptr::null_mut(),
            scintilla_main_handle: core::ptr::null_mut(),
            scintilla_second_handle: core::ptr::null_mut(),
        };
        let result = host.load(99, npp_data);
        assert!(result.is_err());
    }
}
