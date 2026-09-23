//! Plugin discovery, loading, and lifecycle.
//!
//! Phase 3 milestone 2 ships:
//!   - directory enumeration (record paths, no `LoadLibrary` yet — DESIGN.md
//!     §6.4 mandates "loading is deferred"),
//!   - lazy load on first user touch (resolve six entry points, call
//!     `setInfo`, call `getFuncsArray`),
//!   - plugin lifecycle state machine (Pending → Loaded / Failed →
//!     `ShuttingDown`).
//!
//! NPPM_*/NPPN_* dispatching, menu integration, and the actual
//! example-hello DLL land in subsequent milestones.
//!
//! Platform-neutral: discovery and lifecycle go through
//! `codepp_platform::DynLib` (whose Windows/Unix arms this crate does
//! not care about) and `std::panic::catch_unwind`, with no OS-specific
//! code. Unconditional since Phase 5's GTK/Linux host port.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use codepp_platform::{has_plugin_extension, DynLib};

use crate::dispatch::{NPPN_READY, NPPN_TBMODIFICATION};
use crate::ffi::{
    BeNotifiedFn, FuncItem, GetFuncsArrayFn, GetNameFn, IsUnicodeFn, MessageProcFn, NppData,
    SCNotification, SciNotifyHeader, SetInfoFn, ShortcutKey,
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
    /// `true` if the user has marked this plugin as disabled via the
    /// Plugin Manager. Disabled plugins are still surfaced in the
    /// manager UI (so the user can re-enable them) but the lazy-load
    /// path skips them — `LoadLibraryW` is never called.
    ///
    /// Persisted across launches in `<plugins_config_dir>/disabled.txt`
    /// (one filename per line). Toggling the flag at runtime takes
    /// effect on the *next* launch — already-loaded plugins stay
    /// loaded for the rest of the session, matching Notepad++'s
    /// "restart required" semantics.
    pub disabled: bool,
}

impl PluginInfo {
    /// True if the plugin has been loaded (`LoadLibrary`'d, six entry
    /// points resolved, getFuncsArray called).
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        matches!(self.state, PluginState::Loaded(_))
    }

    /// True if a load attempt failed. `reason` carries the diagnostic.
    #[must_use]
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
    #[must_use]
    pub fn display_label(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        self.path.file_stem().and_then(|s| s.to_str()).map_or_else(
            || "<unnamed plugin>".to_string(),
            std::string::ToString::to_string,
        )
    }

    /// Functions the plugin contributed to the Plugins menu, if it has
    /// been loaded.
    #[must_use]
    pub fn func_items(&self) -> Option<&[FuncItem]> {
        if let PluginState::Loaded(p) = &self.state {
            Some(&p.funcs)
        } else {
            None
        }
    }

    /// The `p_sh_key` accelerators snapshotted at load time,
    /// index-aligned with [`Self::func_items`]. `None` per entry
    /// means the plugin declared no accelerator for that command.
    #[must_use]
    pub fn shortcut_defaults(&self) -> Option<&[Option<ShortcutKey>]> {
        if let PluginState::Loaded(p) = &self.state {
            Some(&p.shortcut_defaults)
        } else {
            None
        }
    }

    /// The plugin's file name on disk (`mimeTools.dll`), the same
    /// basename `disabled.txt` and `shortcuts.xml` key on. Empty
    /// only for a path with no final component, which discovery
    /// cannot produce.
    #[must_use]
    pub fn filename(&self) -> String {
        self.path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string()
    }

    /// `beNotified` entry point if loaded. The dispatcher (next
    /// milestone) iterates plugins and calls this with each
    /// `SCNotification` it wants to deliver.
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
    pub fn get_name_fn(&self) -> Option<GetNameFn> {
        if let PluginState::Loaded(p) = &self.state {
            Some(p.get_name)
        } else {
            None
        }
    }

    /// `getFuncsArray` entry point if loaded. Re-callable for plugins
    /// that mutate their menu set after init (rare; mostly Phase 4+).
    #[must_use]
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

/// State of a successfully loaded plugin. Holds the `DynLib` (drops
/// `FreeLibrary` at shutdown), the resolved entry-point function
/// pointers, and the cached `FuncItem` array.
///
/// Public only because it travels from [`execute_load`] to
/// [`PluginHost::commit_load`] through the caller, which is what lets
/// the load itself run with no host borrow held — its fields stay
/// private.
pub struct LoadedPlugin {
    /// The `DynLib`'s job is to keep the underlying DLL mapped for
    /// as long as the plugin is loaded. Clippy does not count `Drop`
    /// as a field-read, so the field appears unread to dead-code
    /// analysis; the allow attribute documents the intentional
    /// ownership.
    ///
    /// **Its `Drop` — which would `FreeLibrary` — is deliberately
    /// never reached.** `PluginHost::drop` destructures this struct
    /// and `mem::forget`s the library; see that impl for why
    /// unloading a plugin at teardown crashes the host.
    #[allow(dead_code)]
    lib: DynLib,
    set_info: SetInfoFn,
    get_name: GetNameFn,
    get_funcs_array: GetFuncsArrayFn,
    be_notified: BeNotifiedFn,
    message_proc: MessageProcFn,
    is_unicode: IsUnicodeFn,
    /// Snapshot of the `FuncItem` array the plugin returned. Each
    /// `FuncItem` is `Copy` (no heap pointers we own — `p_sh_key` is
    /// owned by the plugin), so cloning is safe.
    funcs: Vec<FuncItem>,
    /// The `p_sh_key` accelerators, dereferenced and copied at load
    /// time — index-aligned with `funcs`. Snapshotting here (rather
    /// than letting consumers chase the raw pointer later) keeps the
    /// one unsafe dereference next to the `FuncItem` copy it shares a
    /// validity argument with; everything downstream sees plain
    /// values. `None` = no accelerator (NULL pointer, or a
    /// zero-`_key` struct, which no real chord uses).
    shortcut_defaults: Vec<Option<ShortcutKey>>,
    /// Plugin's getName return value, decoded to UTF-8.
    name: String,
}

/// First menu-command id assigned to a plugin's `FuncItem`. The
/// numeric range starts well above any plausible host-built-in id
/// (Code++'s File menu uses 1000-series ids; Notepad++'s `IDM_BASE`
/// is 40000) so plugin cmds never collide with the host's `WM_COMMAND`
/// handlers in either ABI.
pub const PLUGIN_CMD_ID_BASE: i32 = 50_000;

/// First plugin-allocatable command id. Distinct from
/// [`PLUGIN_CMD_ID_BASE`] so menu-driven plugin commands and
/// programmatically-allocated ones can't collide. Drives
/// `NPPM_ALLOCATECMDID`. The 10 000-id gap above
/// `PLUGIN_CMD_ID_BASE` accommodates 500+ plugins each
/// contributing 20 menu items before the `FuncItem` range would
/// reach the allocator base.
pub const PLUGIN_ALLOC_CMD_BASE: i32 = 60_000;

/// First id past the plugin-allocatable command range. Sized so
/// `PLUGIN_ALLOC_CMD_LIMIT - PLUGIN_ALLOC_CMD_BASE` (5500 ids)
/// is comfortably above any plausible plugin's needs and stays
/// well clear of `u16::MAX` where Win32 `WM_COMMAND` IDs live.
pub const PLUGIN_ALLOC_CMD_LIMIT: i32 = 65_500;

/// First plugin-allocatable Scintilla marker number. Marker 24
/// is reserved for bookmarks (`NPPM_GETBOOKMARKID`); markers
/// 0..=23 are reserved for built-in editor decorations
/// (line-change indicators, breakpoints, error glyphs in future
/// phases). Plugin allocations therefore start at 25.
pub const PLUGIN_ALLOC_MARKER_BASE: i32 = 25;

/// First marker number past the plugin-allocatable range.
/// Scintilla supports markers 0..=31, so the allocatable pool
/// runs 25..=31 — seven markers total. Plugins requesting more
/// in a single call (or once the pool is partially drained) get
/// a clean `false` return; the upstream contract makes no
/// guarantee about pool size, so plugins must handle failure.
pub const PLUGIN_ALLOC_MARKER_LIMIT: i32 = 32;

/// Top-level plugin registry. Owned by the shell; UI crates poke it
/// through `Shell` to enumerate, load, dispatch.
pub struct PluginHost {
    /// Every discovered plugin. **Entries are never removed and a
    /// loaded plugin is never unloaded** — not while the host lives,
    /// and not by the host's own drop either, which deliberately
    /// leaks the libraries rather than `FreeLibrary`ing them (see
    /// `impl Drop for PluginHost`). The UI relies on the first half
    /// of that: `crate::dispatch::NotifyTargets` snapshots the loaded
    /// plugins' `beNotified` pointers and calls them after the borrow
    /// on this host has been dropped. A future unload / hot-reload
    /// path must invalidate those snapshots first.
    plugins: Vec<PluginInfo>,
    /// Next menu-command id to hand out at the next successful load.
    /// Monotonically increasing; never reused so that a plugin which
    /// fails to load cannot leak its allocated cmds onto a later
    /// plugin's items.
    next_cmd_id: i32,
    /// Set between [`PluginHost::next_pending_load`] and its
    /// [`PluginHost::commit_load`].
    ///
    /// The load runs with **no host borrow held**, which is the whole
    /// point of that split — and that is exactly what makes a nested
    /// load reachable: the plugin's own `setInfo` can send the host a
    /// `WM_COMMAND` that walks back into the loader. Without this
    /// latch the nested pass would hand out a second `PendingLoad`
    /// carrying the *same* `cmd_id_base`, because the base only
    /// advances at commit — two plugins would then claim one id
    /// range and a menu click would fire the wrong command. Nested
    /// passes see `None` and do nothing; the outer loop continues.
    ///
    /// **It is cleared only by [`PluginHost::commit_load`], so a
    /// caller that takes a `PendingLoad` and never commits it leaves
    /// loading disabled for the rest of the process.** That is a
    /// window the old single-borrow load did not have. Every backend
    /// commits on both arms of the load, so the reachable trigger is
    /// the state borrow failing between the two phases — i.e. the
    /// window being torn down under a `setInfo` — where the process
    /// is on its way out anyway. Each backend logs at `error` if it
    /// ever happens, because the symptom otherwise is "plugins
    /// silently stopped loading" with nothing to go on.
    load_in_progress: bool,
    /// Next id to hand out for `NPPM_ALLOCATECMDID`. Distinct
    /// from `next_cmd_id` so plugin menu commands and
    /// programmatically-allocated ids can't collide. Bumped by
    /// the requested count on each successful allocation; never
    /// rolled back (allocations are durable for the host's
    /// lifetime — the upstream contract does not define a
    /// deallocate path).
    next_alloc_cmd_id: i32,
    /// Next Scintilla marker number to hand out for
    /// `NPPM_ALLOCATEMARKER`. Same monotonic, never-reused
    /// semantics as `next_alloc_cmd_id`.
    next_alloc_marker: i32,
}

impl Default for PluginHost {
    fn default() -> Self {
        Self {
            plugins: Vec::new(),
            next_cmd_id: PLUGIN_CMD_ID_BASE,
            next_alloc_cmd_id: PLUGIN_ALLOC_CMD_BASE,
            next_alloc_marker: PLUGIN_ALLOC_MARKER_BASE,
            load_in_progress: false,
        }
    }
}

impl PluginHost {
    /// Construct an empty registry. Call [`PluginHost::discover`] to
    /// populate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve `count` consecutive menu-command IDs for the
    /// calling plugin. Drives `NPPM_ALLOCATECMDID`. Returns the
    /// starting id on success, `None` on:
    ///   - `count <= 0` (malformed plugin input);
    ///   - `start + count > PLUGIN_ALLOC_CMD_LIMIT` (pool
    ///     exhausted or single allocation too large to fit);
    ///   - integer overflow on the bound calculation (handled
    ///     via `checked_add`).
    ///
    /// Allocations are durable for the host's lifetime — the
    /// upstream contract does not define a deallocate path.
    pub fn allocate_cmd_id(&mut self, count: i32) -> Option<i32> {
        if count <= 0 {
            return None;
        }
        let start = self.next_alloc_cmd_id;
        let end = start.checked_add(count)?;
        if end > PLUGIN_ALLOC_CMD_LIMIT {
            tracing::warn!(
                requested = count,
                next_id = start,
                limit = PLUGIN_ALLOC_CMD_LIMIT,
                "NPPM_ALLOCATECMDID: pool exhausted"
            );
            return None;
        }
        self.next_alloc_cmd_id = end;
        Some(start)
    }

    /// Reserve `count` consecutive Scintilla marker numbers for
    /// the calling plugin. Drives `NPPM_ALLOCATEMARKER`. Same
    /// success / failure shape as [`Self::allocate_cmd_id`]; the
    /// pool is much smaller (seven markers, 25..=31) so even
    /// modest plugins can exhaust it.
    pub fn allocate_marker(&mut self, count: i32) -> Option<i32> {
        if count <= 0 {
            return None;
        }
        let start = self.next_alloc_marker;
        let end = start.checked_add(count)?;
        if end > PLUGIN_ALLOC_MARKER_LIMIT {
            tracing::warn!(
                requested = count,
                next_id = start,
                limit = PLUGIN_ALLOC_MARKER_LIMIT,
                "NPPM_ALLOCATEMARKER: pool exhausted"
            );
            return None;
        }
        self.next_alloc_marker = end;
        Some(start)
    }

    /// Enumerate plugin candidates in `dir`. Each `*.dll` becomes a
    /// `PluginInfo` in the `Pending` state — the file is **not** yet
    /// `LoadLibrary`'d. Returns the count discovered.
    ///
    /// A non-existent directory is not an error; it's the first-run
    /// case. The scan walks **two** subdirectory levels deep so all
    /// three of these layouts are picked up:
    ///
    ///   plugins/<name>.dll                       (depth 0)
    ///   plugins/<name>/<name>.dll                (depth 1, the
    ///                                             Notepad++ default)
    ///   plugins/<name>/<archdir>/<name>.dll      (depth 2, the
    ///                                             `NppExec` /
    ///                                             `ComparePlus`
    ///                                             64-bit layout)
    ///
    /// At depth ≥ 1 the candidate's filename stem must match the
    /// plugin's directory name (`is_plugin_dll`). Without that filter
    /// a plugin's bundled dependencies (e.g. `ComparePlus` shipping
    /// `git2.dll` and `sqlite3.dll` under `libs/`) would be picked up
    /// as plugins themselves, fed to `LoadLibraryW` at first-touch
    /// load, and either fail entry-point resolution noisily (best
    /// case) or run their `DllMain` and bring foreign DLL state into
    /// the host process (worst case). The N++ convention this filter
    /// mirrors is the same protection.
    ///
    /// Symlinks: `is_dir()`/`is_file()` follow symlinks, so a
    /// directory symlink in the plugins folder is enumerated. On
    /// Windows symlink creation requires `SeCreateSymbolicLinkPrivilege`
    /// by default, so this is low-severity. Phase 5 (Linux/macOS,
    /// where symlink creation is unprivileged) will need to validate
    /// resolved paths stay within `dir` or use `O_NOFOLLOW`.
    ///
    /// # Errors
    ///
    /// Currently the recursive `discover_walk` absorbs every
    /// read-dir failure (matching the "no plugins folder yet"
    /// first-run case), so this signature is `Result` mostly for
    /// forward-compat with a future stricter mode. Today it
    /// always returns `Ok`.
    pub fn discover(&mut self, dir: &Path) -> std::io::Result<usize> {
        // No `exists()` pre-check: a separate stat-then-open opens a
        // TOCTOU window where an attacker who can swap `dir` for a
        // symlink between the check and the `read_dir` call could
        // redirect enumeration into a directory of their choosing,
        // with the recorded paths later fed to `LoadLibraryW` at
        // first-touch load. `discover_walk` already treats a
        // missing-directory `read_dir` failure as "no entries"
        // (matching the first-run case), so the redundant pre-check
        // adds the race without buying anything.
        let mut found = 0usize;
        self.discover_walk(dir, 0, 2, &mut found);
        Ok(found)
    }

    fn discover_walk(&mut self, dir: &Path, depth: u32, max_depth: u32, found: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && has_plugin_extension(&path) && is_plugin_dll(&path, depth) {
                self.plugins.push(PluginInfo {
                    path,
                    name: None,
                    state: PluginState::Pending,
                    // Default to enabled at discovery time. The
                    // shell sweeps `apply_disabled_list` over the
                    // registry once enumeration finishes, flipping
                    // `disabled = true` for any DLL whose filename
                    // appears in `disabled.txt`.
                    disabled: false,
                });
                *found += 1;
            } else if path.is_dir() && depth < max_depth {
                self.discover_walk(&path, depth + 1, max_depth, found);
            }
        }
    }

    /// Total number of plugins (any state).
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// True if no plugins are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Iterate all known plugins regardless of lifecycle state.
    pub fn iter(&self) -> impl Iterator<Item = &PluginInfo> {
        self.plugins.iter()
    }

    /// Apply the on-disk "disabled plugins" list to the registry.
    /// `disabled_filenames` is the set of DLL filenames (basename
    /// only, case-insensitive on Windows) that should be marked
    /// disabled. Any plugin whose filename matches gets
    /// `disabled = true`; anything not on the list is left alone
    /// (so toggling at runtime persists across the next discover).
    ///
    /// Called by the shell once after `discover` completes, with
    /// the contents of `<plugins_config_dir>/disabled.txt`. Empty
    /// or missing file → empty set → all plugins enabled.
    pub fn apply_disabled_list(&mut self, disabled_filenames: &[String]) {
        for plugin in &mut self.plugins {
            let basename = plugin
                .path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            plugin.disabled = disabled_filenames.iter().any(|d| filenames_eq(d, basename));
        }
    }

    /// Mark the plugin at index `idx` as disabled or enabled.
    /// Returns `true` if the state actually changed (caller can
    /// use this to skip writing `disabled.txt` when nothing
    /// moved). Out-of-range index is a silent no-op returning
    /// `false`.
    pub fn set_disabled(&mut self, idx: usize, disabled: bool) -> bool {
        let Some(plugin) = self.plugins.get_mut(idx) else {
            return false;
        };
        if plugin.disabled == disabled {
            return false;
        }
        plugin.disabled = disabled;
        true
    }

    /// Snapshot the registry as a list of `(index, basename,
    /// display_label, disabled)` tuples — the shape the Plugin
    /// Manager UI consumes. Sorted by display label so the
    /// listview shows a stable, alphabetised view.
    #[must_use]
    pub fn snapshot_for_admin(&self) -> Vec<PluginAdminEntry> {
        let mut out: Vec<PluginAdminEntry> = self
            .plugins
            .iter()
            .enumerate()
            .map(|(idx, p)| PluginAdminEntry {
                index: idx,
                filename: p.filename(),
                display_label: p.display_label(),
                path: p.path.clone(),
                disabled: p.disabled,
                loaded: p.is_loaded(),
                failed_reason: p.failed_reason().map(std::string::ToString::to_string),
            })
            .collect();
        out.sort_by(|a, b| {
            a.display_label
                .to_ascii_lowercase()
                .cmp(&b.display_label.to_ascii_lowercase())
        });
        out
    }

    /// The next plugin that wants loading, taken under the host
    /// borrow so the caller can then drop it.
    ///
    /// **Why this is three calls and not one.** `setInfo` and
    /// `getFuncsArray` are foreign code, and a real plugin uses them
    /// to interrogate the host — `NppExec` asks for the version in
    /// `setInfo` and refuses to start if it cannot get one. A host
    /// that holds `&mut` state across that call has to decline the
    /// re-entrant query (the alternative is aliasing), so the plugin
    /// reads 0 and draws the wrong conclusion. The fix is not to hold
    /// the borrow: take what the load needs here, run the foreign
    /// code with nothing borrowed ([`execute_load`]), then commit
    /// ([`Self::commit_load`]). Same shape as
    /// `Shell::begin_close_active_tab` / `close_announced_tab`.
    ///
    /// Returns `None` when every plugin is loaded, failed or
    /// disabled. Call it in a loop, committing each result before
    /// asking for the next, because the command-id base each plugin
    /// gets depends on how many `FuncItem`s its predecessors
    /// published.
    pub fn next_pending_load(&mut self) -> Option<PendingLoad> {
        if self.load_in_progress {
            // A nested loader — see the field doc. Answering here
            // would duplicate a command-id range.
            return None;
        }
        let cmd_id_base = self.next_cmd_id;
        let (idx, plugin) = self
            .plugins
            .iter()
            .enumerate()
            .find(|(_, p)| !p.is_loaded() && p.failed_reason().is_none() && !p.disabled)?;
        let pending = PendingLoad {
            idx,
            path: plugin.path.clone(),
            cmd_id_base,
        };
        self.load_in_progress = true;
        Some(pending)
    }

    /// Record the outcome of an [`execute_load`], and hand back the
    /// notifications the caller must deliver **after** dropping the
    /// host borrow.
    ///
    /// `NPPN_READY` and `NPPN_TBMODIFICATION` are delivered by the
    /// caller rather than here for the same reason `setInfo` is: a
    /// plugin that queries the host from `NPPN_READY` (its config
    /// directory, the version) is doing something ordinary, and it
    /// can only be answered if nothing is borrowed.
    ///
    /// The `Ok(None)` arm is unreachable today — a successful load
    /// always yields a `PluginReady`. It exists so a future load that
    /// legitimately has nothing to notify (a plugin resolved from a
    /// cache, say) does not have to change this signature and every
    /// backend with it.
    ///
    /// # Errors
    ///
    /// The load itself failed (the error is recorded on the plugin
    /// and surfaced to the UI), or `pending.idx` is out of range.
    pub fn commit_load(
        &mut self,
        pending: &PendingLoad,
        result: Result<LoadedPlugin, String>,
    ) -> Result<Option<PluginReady>, String> {
        self.load_in_progress = false;
        let Some(plugin) = self.plugins.get_mut(pending.idx) else {
            return Err(format!("plugin index {} out of range", pending.idx));
        };
        // `PendingLoad` has to be public to cross the borrow gap, so
        // its fields are constructible by any caller. A value whose
        // `idx` and `path` disagree would file one library's
        // `LoadedPlugin` under another plugin's entry — the registry
        // would then report the wrong name, and a menu click would
        // reach the wrong DLL. Nothing does this today; the check
        // costs a string compare once per load.
        if plugin.path != pending.path {
            return Err(format!(
                "PendingLoad for {} does not match plugin {} at {}",
                pending.path.display(),
                pending.idx,
                plugin.path.display()
            ));
        }
        match result {
            Ok(loaded) => {
                // Reserve the assigned ids — never reused, even if a
                // later plugin fails to load and never publishes its
                // FuncItems.
                self.next_cmd_id = self.next_cmd_id.saturating_add(loaded.funcs.len() as i32);
                let be_notified = loaded.be_notified;
                plugin.name = Some(loaded.name.clone());
                plugin.state = PluginState::Loaded(loaded);
                Ok(Some(PluginReady {
                    path: pending.path.clone(),
                    be_notified,
                }))
            }
            Err(e) => {
                plugin.state = PluginState::Failed(e.clone());
                Err(e)
            }
        }
    }

    /// Load one plugin end to end in a single call.
    ///
    /// **Not for a UI backend.** This holds the host borrow across
    /// the plugin's `setInfo`, `getFuncsArray` and `NPPN_READY`,
    /// which is precisely what the three-phase API
    /// ([`Self::next_pending_load`] → [`execute_load`] →
    /// [`Self::commit_load`]) exists to avoid: a plugin that
    /// interrogates the host from `setInfo` gets declined, reads 0,
    /// and — in `NppExec`'s case — refuses to start. A backend calling
    /// this reintroduces that bug.
    ///
    /// It is here for callers with no UI state to alias, which in
    /// practice means the test harnesses: they own the `PluginHost`
    /// outright, so there is no second borrow for a re-entrant call
    /// to collide with.
    ///
    /// # Errors
    ///
    /// The library failed to map, an entry point was missing, or the
    /// plugin published a malformed `FuncItem` array.
    pub fn load_blocking(
        &mut self,
        idx: usize,
        npp_data: NppData,
        dispatch: Option<crate::ffi::HostDispatchFn>,
    ) -> Result<(), String> {
        let Some(plugin) = self.plugins.get(idx) else {
            return Err(format!("plugin index {idx} out of range"));
        };
        if plugin.is_loaded() || plugin.disabled {
            return Ok(());
        }
        let pending = PendingLoad {
            idx,
            path: plugin.path.clone(),
            cmd_id_base: self.next_cmd_id,
        };
        let loaded = execute_load(&pending, npp_data, dispatch);
        let ready = self.commit_load(&pending, loaded)?;
        if let Some(ready) = ready {
            ready.deliver(npp_data.npp_handle);
        }
        Ok(())
    }

    /// Find the `FuncItem` matching `cmd_id` across all loaded plugins
    /// and return its callback. The callback is a plain C function
    /// pointer; the caller must invoke it from the UI thread (parity
    /// with Notepad++) and may want to wrap the call in
    /// `catch_unwind` to keep panics from unwinding across the FFI.
    #[must_use]
    pub fn lookup_cmd(&self, cmd_id: i32) -> Option<crate::ffi::PluginCmd> {
        for plugin in &self.plugins {
            if let Some(funcs) = plugin.func_items() {
                for f in funcs {
                    if f.cmd_id == cmd_id {
                        return f.p_func;
                    }
                }
            }
        }
        None
    }
}

impl Drop for PluginHost {
    /// **Deliberately does not unload the plugin libraries.**
    ///
    /// `DynLib`'s own `Drop` calls `FreeLibrary`, and running it here
    /// — at process teardown, which is the only place a `PluginHost`
    /// is dropped — crashes the host. Measured rather than reasoned
    /// about: with the real `NppExec` loaded, Code++ exited
    /// `0xC0000005` (`STATUS_ACCESS_VIOLATION`) every time, and
    /// skipping the `FreeLibrary` made it exit `0` every time, with
    /// no other change.
    ///
    /// The reason is that a host cannot know what a plugin still has
    /// alive. Unmapping the library leaves any window whose `WNDPROC`
    /// lives in it, any `SetTimer` callback, any thread, any hook and
    /// any TLS destructor pointing at addresses that are no longer
    /// mapped — and Windows keeps delivering to them. Our own dock
    /// frames make one instance of that certain rather than likely:
    /// they are *owned* by the main window, so Win32 destroys them
    /// after the main window's `WM_DESTROY` returns, which is after
    /// this drop runs, and destroying a frame destroys the plugin's
    /// client window with it. But `NppExec` crashes without registering
    /// a dock at all, so enumerating the cases is not a strategy.
    ///
    /// What is lost by not unloading: nothing the OS does not do a
    /// moment later. The process is exiting; every mapping goes with
    /// it. `NPPN_SHUTDOWN` still fires (from each backend's
    /// `WM_DESTROY`, while everything is still mapped), so a plugin
    /// still gets its documented chance to save state.
    ///
    /// This is the one place the host deviates from DESIGN.md §6.4's
    /// "on exit: `NPPN_SHUTDOWN` → unload", and §6.4 records why.
    ///
    /// Applied on every platform, but **measured only on Windows** —
    /// the repro needs a real third-party plugin and there is no
    /// Linux or macOS runner on the development host. The POSIX
    /// analogue of the hazard is real (an `atexit` handler, a
    /// pthread TLS destructor or a signal handler registered by a
    /// `dlclose`d `.so`), so the unconditional choice is the
    /// conservative one rather than a verified one.
    ///
    /// **The leak is per drop, not per process**, and nothing here
    /// enforces that a `PluginHost` is dropped only at exit — an
    /// assertion to that effect was tried and removed, because the
    /// test suite legitimately builds and drops many hosts in one
    /// process and it fired on all of them. Each backend builds
    /// exactly one `Shell`, so the invariant holds today by
    /// construction. A future hot-reload or restart-without-exit path
    /// would make this leak per cycle and unbounded, silently; such a
    /// path needs a real unload story (drain the plugin's windows and
    /// timers first, then `FreeLibrary`) rather than this.
    fn drop(&mut self) {
        for plugin in &mut self.plugins {
            let state = std::mem::replace(&mut plugin.state, PluginState::Pending);
            if let PluginState::Loaded(loaded) = state {
                // Leak *only* the library. The rest of `LoadedPlugin`
                // — the cached `FuncItem`s, the shortcut defaults,
                // the name — is ordinary host-owned heap with no
                // relationship to the unmapped-module hazard, so it
                // drops normally. Destructuring rather than
                // `mem::forget`ing the whole value is what keeps the
                // leak to the thing the reasoning is actually about.
                let LoadedPlugin { lib, .. } = loaded;
                std::mem::forget(lib);
            }
        }
    }
}

/// A plugin picked for loading by [`PluginHost::next_pending_load`],
/// carrying everything [`execute_load`] needs so the host borrow can
/// be dropped before any plugin code runs.
#[derive(Clone, Debug)]
pub struct PendingLoad {
    /// Index into the host's plugin list, passed back to
    /// [`PluginHost::commit_load`].
    pub idx: usize,
    /// The library to map.
    pub path: std::path::PathBuf,
    /// First command id this plugin's `FuncItem`s get.
    pub cmd_id_base: i32,
}

/// A freshly-loaded plugin's `beNotified`, handed back by
/// [`PluginHost::commit_load`] so the caller can fire the load-time
/// notifications with no host borrow held. See
/// [`PluginReady::deliver`].
#[derive(Clone, Debug)]
pub struct PluginReady {
    /// Only for the log line on a panicking handler.
    path: std::path::PathBuf,
    be_notified: crate::ffi::BeNotifiedFn,
}

impl PluginReady {
    /// Fire `NPPN_READY` then `NPPN_TBMODIFICATION` at this plugin.
    ///
    /// N++ broadcasts READY once after all static plugins finish
    /// initialising; Code++ loads lazily, so per-plugin delivery at
    /// load time is the closest equivalent — each plugin sees READY
    /// the moment it is actually ready to handle host messages, never
    /// twice, and a plugin loaded later does not re-broadcast at its
    /// already-initialised peers. TBMODIFICATION immediately follows,
    /// which is N++'s order and the window in which a plugin
    /// registers toolbar icons.
    ///
    /// **Call this with no host borrow held.** That is the whole
    /// point of the type: a plugin's handler may send `NPPM_*` back
    /// at the host, and it can only be answered if nothing is
    /// borrowed.
    pub fn deliver(&self, npp_handle: crate::ffi::Hwnd) {
        for code in [NPPN_READY, NPPN_TBMODIFICATION] {
            let sci = SCNotification {
                nmhdr: SciNotifyHeader {
                    hwnd_from: npp_handle,
                    id_from: 0,
                    code,
                },
                ..SCNotification::default()
            };
            // SAFETY: `be_notified` came from a successful resolve in
            // `execute_load`, its library is still mapped (plugins are
            // never unloaded before the host drops), and the
            // `SCNotification` is `#[repr(C)]` and lives on this stack
            // frame through the synchronous call.
            let result = catch_unwind(AssertUnwindSafe(|| unsafe {
                (self.be_notified)(&raw const sci);
            }));
            if result.is_err() {
                // Same warn-on-panic posture as `notify_all`: the
                // caller is told the load succeeded either way, so a
                // plugin that dies during its own init would
                // otherwise be invisible.
                tracing::warn!(path = ?self.path, code = code, "plugin panicked in beNotified");
            }
        }
    }
}

/// Map a plugin and run its `setInfo` / `getFuncsArray`.
///
/// **Takes no host state on purpose** — see
/// [`PluginHost::next_pending_load`]. Everything this touches is the
/// plugin's own library, so a re-entrant `NPPM_*` from inside
/// `setInfo` finds the host unborrowed and gets a real answer.
///
/// # Errors
///
/// The library failed to map, an entry point was missing, or the
/// plugin published a malformed `FuncItem` array.
pub fn execute_load(
    pending: &PendingLoad,
    npp_data: NppData,
    dispatch: Option<crate::ffi::HostDispatchFn>,
) -> Result<LoadedPlugin, String> {
    let _span = tracing::info_span!("plugin_load", path = ?pending.path).entered();
    load_inner(&pending.path, npp_data, pending.cmd_id_base, dispatch)
}

/// One row's worth of data for the Plugin Manager UI. Decoupled
/// from `PluginInfo` so the UI doesn't take a borrow on the host
/// across the modal pump (we'd otherwise hold `&PluginHost`
/// through `IsDialogMessageW` and break the standard re-entrance
/// rule).
#[derive(Clone, Debug)]
pub struct PluginAdminEntry {
    /// Index into `PluginHost.plugins` — the Plugin Manager
    /// passes this back via `set_disabled` when the user toggles
    /// a row. Stable for the lifetime of the host (we never
    /// remove plugins from the registry).
    pub index: usize,
    /// DLL filename (basename, including `.dll` extension). The
    /// canonical key written into `disabled.txt`.
    pub filename: String,
    /// User-facing label — `getName()` for loaded plugins, file
    /// stem for unloaded ones.
    pub display_label: String,
    /// Full path to the DLL — UI uses this to read the PE
    /// VERSIONINFO resource for the version column.
    pub path: PathBuf,
    /// Current disabled flag. Snapshot only; the UI writes
    /// changes through `Shell::set_plugin_disabled`.
    pub disabled: bool,
    /// True iff the plugin's DLL is currently mapped into the
    /// process. UI shows a hint that disabling a loaded plugin
    /// requires a restart for the change to fully take effect.
    pub loaded: bool,
    /// `Some(reason)` if a load attempt failed; `None` otherwise.
    /// UI surfaces the reason as a tooltip on the row.
    pub failed_reason: Option<String>,
}

/// Case-insensitive filename comparison — matches Windows' NTFS
/// behaviour so a `disabled.txt` entry of `ComparePlus.dll`
/// matches a DLL file `compareplus.dll` on disk.
fn filenames_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Resolve the six entry points and run the initial setInfo +
/// getFuncsArray dance. Returns a fully-populated `LoadedPlugin` on
/// success. `cmd_id_base` is the first menu-command id assigned to
/// the plugin's `FuncItems` — incremented by one per item, written
/// back through the plugin's pointer so the plugin's own copy of
/// `_cmdID` matches the value the host installs in the menu.
/// Install the host's message-routing callback into a freshly-loaded
/// plugin, so its `SendMessage` transport reaches the host.
///
/// Only meaningful off Windows: there the plugin exports
/// `codepp_plugin_set_dispatch` (from `codepp-plugin-sdk`) and has no OS
/// message pump. On Windows `dispatch` is `None` and the symbol is
/// absent (the SDK's export is `#[cfg(not(windows))]`), so this is a
/// no-op. A plugin that doesn't use our SDK simply won't have the symbol
/// and can't talk back to the host — acceptable, since Linux only ever
/// loads SDK-built cdylibs.
fn install_dispatch(lib: &DynLib, dispatch: Option<crate::ffi::HostDispatchFn>, path: &Path) {
    let Some(dispatch) = dispatch else {
        return;
    };
    // SAFETY: `SetDispatchFn` is the ABI of the SDK's export.
    let set_dispatch =
        unsafe { lib.resolve::<crate::ffi::SetDispatchFn>("codepp_plugin_set_dispatch") };
    let Some(set_dispatch) = set_dispatch else {
        tracing::debug!(
            path = ?path,
            "plugin exports no codepp_plugin_set_dispatch; it cannot send messages to the host",
        );
        return;
    };
    // SAFETY: resolved to the SDK's `codepp_plugin_set_dispatch`, which
    // stores the pointer in an atomic — no unwinding, no retained borrow.
    unsafe { set_dispatch(Some(dispatch)) };
}

fn load_inner(
    path: &Path,
    npp_data: NppData,
    cmd_id_base: i32,
    dispatch: Option<crate::ffi::HostDispatchFn>,
) -> Result<LoadedPlugin, String> {
    // Cap on the FuncItem count a plugin can contribute. Hoisted
    // above all statements (clippy's `items_after_statements`)
    // so the constant declaration sits with the function's
    // documentation rather than appearing mid-body. The cap is
    // a DoS guard — `i32::MAX` from a hostile or broken plugin
    // would otherwise trigger a ~17 GB `Vec::with_capacity`.
    const MAX_FUNCITEMS: i32 = 1024;

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

    // Install the host's message-routing callback *before* setInfo, so
    // the plugin's `SendMessage` transport is live for every message it
    // could send once initialised (NPPN_READY's beNotified, menu
    // commands).
    install_dispatch(&lib, dispatch, path);

    // setInfo first — plugin stashes the host handles before we ask
    // it for menu items. Wrap each FFI call in `catch_unwind` so a
    // Rust-authored plugin that panics doesn't unwind across the C
    // ABI (that's UB; DESIGN.md §6.5). C++ plugins that throw past
    // their own ABI are out of scope — broken in Notepad++ too.
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
        unsafe { get_funcs_array(&raw mut count) }
    }))
    .map_err(|_| "plugin panicked in getFuncsArray".to_string())?;
    // Cap implausible counts (see `MAX_FUNCITEMS` at the top of
    // this function for rationale).
    if count > MAX_FUNCITEMS {
        return Err(format!(
            "getFuncsArray returned implausible count {count}; cap is {MAX_FUNCITEMS}"
        ));
    }
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
            shortcut_defaults: Vec::new(),
            name,
        });
    }

    // Copy the FuncItem array out of the plugin's address space into
    // our own Vec, assigning each entry a host-allocated `cmd_id`
    // and writing that id back through the plugin's pointer so the
    // plugin's own copy of `_cmdID` matches what the host installs
    // in the menu (the ABI contract from PluginInterface.h). The
    // plugin retains ownership of its FuncItem memory and the
    // `p_sh_key` accelerator pointers; we copy by value.
    // SAFETY: raw is non-null and points to `count` valid FuncItem
    // values (per the plugin's contract). We read and write each
    // element. Plugins that store FuncItems in read-only memory
    // (e.g. as a const initializer in the .rdata section) cause an
    // access violation at the write — Notepad++ has the same
    // requirement, so this matches the public ABI.
    let (funcs, shortcut_defaults) = unsafe {
        let count = count as usize;
        let mut out = Vec::with_capacity(count);
        let mut shortcuts = Vec::with_capacity(count);
        for i in 0..count {
            let id = cmd_id_base.saturating_add(i as i32);
            // Write the id back through the plugin's pointer first,
            // then read the (now-updated) entry by value into our
            // Vec — guarantees our copy and the plugin's copy agree.
            (*raw.add(i)).cmd_id = id;
            out.push(*raw.add(i));
            // Dereference the accelerator now, while the DLL is
            // freshly mapped and under the same validity contract
            // as the FuncItem read above (PluginInterface.h: the
            // ShortcutKey outlives the plugin until SHUTDOWN).
            shortcuts.push(snapshot_shortcut_key((*raw.add(i)).p_sh_key));
        }
        (out, shortcuts)
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
        shortcut_defaults,
        name,
    })
}

/// Copy a plugin's `ShortcutKey` out of its address space, or `None`
/// for a NULL pointer or a zero-`key` struct (no real chord binds
/// virtual-key 0; N++ treats it as "unassigned" too). Read is
/// unaligned — the ABI struct is 4 × `u8` so any alignment is legal,
/// and a plugin handing out a pointer into packed storage must not
/// fault the host.
///
/// # Safety
///
/// `p` must be NULL or point to a `ShortcutKey` that is valid for
/// reads — the `FuncItem._pShKey` ABI contract (plugin-owned, lives
/// until shutdown). Callers pass pointers obtained from a
/// just-loaded plugin's `getFuncsArray`.
unsafe fn snapshot_shortcut_key(p: *mut ShortcutKey) -> Option<ShortcutKey> {
    if p.is_null() {
        return None;
    }
    let key = unsafe { p.read_unaligned() };
    if key.key == 0 {
        return None;
    }
    Some(key)
}

/// Decide whether a `*.dll` candidate found at `depth` in the plugins
/// tree is actually a plugin or a bundled dependency. The Notepad++
/// convention is:
///
/// * **depth 0** (`plugins/X.dll`): always a plugin. Code++ allows this
///   layout for convenience even though stock N++ requires the per-
///   plugin subdirectory.
/// * **depth 1** (`plugins/X/Y.dll`): plugin only when `Y == X`. The
///   stem must match the parent directory. This rejects bundled
///   dependencies (`plugins/X/libs/git2.dll` → `plugins/X/libs/`,
///   stem "git2" ≠ parent "libs").
/// * **depth 2** (`plugins/X/<arch>/Y.dll`): plugin only when `Y == X`,
///   i.e. the stem must match the *grandparent* directory (the
///   plugin name), not the immediate `<arch>` parent. This is the
///   `NppExec` / `ComparePlus` 64-bit layout.
///
/// Returns false on any path that lacks the parent / grandparent
/// component the rule needs (defensive — `read_dir` shouldn't produce
/// such paths but the parent component is `Option`-typed).
fn is_plugin_dll(path: &Path, depth: u32) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    // Case-insensitive comparison: NTFS is case-insensitive by
    // default, so a plugin named "ComparePlus" might be returned by
    // read_dir as "Compareplus" or any other casing depending on
    // how it was created. ASCII case-insensitive is enough — plugin
    // names in the wild are ASCII. `dir_matches_stem` is `Fn` (no
    // captured state moved on call) so additional match arms below
    // can call it without consuming it.
    let dir_matches_stem = |dir: Option<&Path>| -> bool {
        dir.and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(stem))
    };
    match depth {
        0 => true,
        1 => dir_matches_stem(path.parent()),
        2 => dir_matches_stem(path.parent().and_then(|p| p.parent())),
        _ => false,
    }
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

// These tests were authored against Windows discovery (`.dll` filenames,
// the stem-matches-dirname walk) and stay Windows-only for now — they
// exercise platform-neutral logic, but parametrising the 23 hardcoded
// `.dll` fixtures on `PLUGIN_EXTENSION` is a tracked follow-up. The
// Linux load path (including the new dispatch handshake) is covered
// end-to-end by the GTK plugin demo, and `has_plugin_extension` is
// already tested per-OS in `codepp-platform`.
#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    /// The one unsafe dereference the shortcut path adds: NULL and
    /// zero-key pointers yield `None`, a real chord copies by value.
    /// (The end-to-end read out of a real DLL's `FuncItem`s is covered
    /// by the windows-gated load tests once a bundled plugin ships a
    /// default shortcut.)
    #[test]
    fn snapshot_shortcut_key_handles_null_zero_and_real_chords() {
        assert_eq!(unsafe { snapshot_shortcut_key(std::ptr::null_mut()) }, None);
        let mut sk = ShortcutKey {
            is_ctrl: 1,
            is_alt: 1,
            is_shift: 0,
            key: 0x48,
        };
        assert_eq!(unsafe { snapshot_shortcut_key(&raw mut sk) }, Some(sk));
        sk.key = 0;
        assert_eq!(unsafe { snapshot_shortcut_key(&raw mut sk) }, None);
    }

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
    fn discover_rejects_bundled_deps_in_libs_subdir() {
        // ComparePlus ships its libs (git2.dll, sqlite3.dll) under
        // plugins/<plugin>/libs/. Without the filename-stem-matches-
        // dirname filter, those would be enumerated as plugins
        // themselves and fed to LoadLibraryW, which can crash the
        // process if the bundled DLL's DllMain runs unexpected
        // code or its later setInfo lookup hits a name collision.
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("ComparePlus");
        let libs = plugin_dir.join("libs");
        std::fs::create_dir_all(&libs).unwrap();
        std::fs::write(plugin_dir.join("ComparePlus.dll"), b"x").unwrap();
        std::fs::write(libs.join("git2.dll"), b"x").unwrap();
        std::fs::write(libs.join("sqlite3.dll"), b"x").unwrap();

        let mut host = PluginHost::new();
        let n = host.discover(dir.path()).unwrap();
        assert_eq!(n, 1, "only ComparePlus.dll should be a plugin");
        assert_eq!(
            host.iter().next().unwrap().path.file_name().unwrap(),
            "ComparePlus.dll"
        );
    }

    #[test]
    fn discover_rejects_misnamed_dll_under_plugin_dir() {
        // plugins/Foo/Bar.dll — stem "Bar" doesn't match parent
        // "Foo", so it's a bundled dependency, not the plugin entry.
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("Foo");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("Bar.dll"), b"x").unwrap();

        let mut host = PluginHost::new();
        let n = host.discover(dir.path()).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn discover_accepts_case_mismatched_dll_name() {
        // NTFS is case-insensitive; the user might have a directory
        // "ComparePlus" containing "compareplus.dll" or vice versa.
        // The filter uses ASCII case-insensitive comparison so the
        // same plugin layout works regardless of how the casing
        // landed in read_dir output.
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("ComparePlus");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("compareplus.dll"), b"x").unwrap();

        let mut host = PluginHost::new();
        let n = host.discover(dir.path()).unwrap();
        assert_eq!(n, 1);
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
        let result = host.load_blocking(0, npp_data, None);
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
        let result = host.load_blocking(99, npp_data, None);
        assert!(result.is_err());
    }

    // --- Plugin-id allocator (NPPM_ALLOCATECMDID / NPPM_ALLOCATEMARKER) ---

    #[test]
    fn allocate_cmd_id_starts_at_pool_base() {
        let mut host = PluginHost::new();
        assert_eq!(host.allocate_cmd_id(1), Some(PLUGIN_ALLOC_CMD_BASE));
        // Next call hands out the slot right after the previous
        // allocation — no overlap.
        assert_eq!(host.allocate_cmd_id(3), Some(PLUGIN_ALLOC_CMD_BASE + 1));
        assert_eq!(host.allocate_cmd_id(2), Some(PLUGIN_ALLOC_CMD_BASE + 4));
    }

    #[test]
    fn allocate_cmd_id_rejects_zero_and_negative_count() {
        let mut host = PluginHost::new();
        assert_eq!(host.allocate_cmd_id(0), None);
        assert_eq!(host.allocate_cmd_id(-5), None);
        // Counter unchanged — a malformed call must not leak the
        // base slot.
        assert_eq!(host.allocate_cmd_id(1), Some(PLUGIN_ALLOC_CMD_BASE));
    }

    #[test]
    fn allocate_cmd_id_pool_exhaustion_keeps_counter_intact() {
        let mut host = PluginHost::new();
        // Eat the whole pool in one big chunk.
        let pool_size = PLUGIN_ALLOC_CMD_LIMIT - PLUGIN_ALLOC_CMD_BASE;
        assert_eq!(host.allocate_cmd_id(pool_size), Some(PLUGIN_ALLOC_CMD_BASE));
        // Pool is now empty — any further request fails without
        // mutating state.
        assert_eq!(host.allocate_cmd_id(1), None);
        // Even a request that "exactly fits" but starts past the
        // limit fails the same way.
        assert_eq!(host.allocate_cmd_id(0), None);
    }

    #[test]
    fn allocate_cmd_id_request_too_large_for_pool_returns_none() {
        let mut host = PluginHost::new();
        // Single allocation larger than the entire pool fails;
        // the counter stays at the base.
        let pool_size = PLUGIN_ALLOC_CMD_LIMIT - PLUGIN_ALLOC_CMD_BASE;
        assert_eq!(host.allocate_cmd_id(pool_size + 1), None);
        // Subsequent normal request succeeds — the failed call
        // didn't burn the base slot.
        assert_eq!(host.allocate_cmd_id(1), Some(PLUGIN_ALLOC_CMD_BASE));
    }

    #[test]
    fn allocate_marker_starts_above_bookmark_slot() {
        let mut host = PluginHost::new();
        // Marker 24 is reserved for `NPPM_GETBOOKMARKID`; the
        // allocator pool starts at 25.
        assert_eq!(host.allocate_marker(1), Some(25));
        assert_eq!(host.allocate_marker(1), Some(26));
    }

    #[test]
    fn allocate_marker_pool_seven_markers_then_exhausts() {
        // Pool runs 25..=31 (seven markers). The eighth single
        // allocation must fail.
        let mut host = PluginHost::new();
        for i in 0..7 {
            assert_eq!(host.allocate_marker(1), Some(25 + i));
        }
        assert_eq!(host.allocate_marker(1), None);
    }

    #[test]
    fn allocate_marker_oversized_request_fails_atomically() {
        let mut host = PluginHost::new();
        // Request larger than the entire pool fails without
        // burning any markers — the alloc is atomic.
        assert_eq!(host.allocate_marker(8), None);
        assert_eq!(host.allocate_marker(1), Some(25));
    }
}

// Windows-only for the same reason as the module above: the
// fixtures are `.dll`-named.
#[cfg(all(test, target_os = "windows"))]
mod load_split_tests {
    //! The three-phase load exists so a plugin's `setInfo` can query
    //! the host. These pin the parts a backend depends on.

    use super::PluginHost;

    /// An empty registry has nothing to hand out. The loop condition
    /// every backend writes depends on this terminating.
    #[test]
    fn an_empty_host_has_nothing_pending() {
        let mut host = PluginHost::default();
        assert!(host.next_pending_load().is_none());
    }

    /// A second `next_pending_load` before the first is committed
    /// answers `None`.
    ///
    /// Not a nicety: the load runs with no host borrow held, so a
    /// plugin's own `setInfo` can send the host a message that walks
    /// back into the loader. Without the latch the nested pass would
    /// hand out a `PendingLoad` carrying the *same* `cmd_id_base` —
    /// the base only advances at commit — and two plugins would claim
    /// one command-id range.
    #[test]
    fn a_nested_load_is_refused_until_the_first_commits() {
        let dir = tempfile::tempdir().unwrap();
        // Two entries so a nested pass would have something to
        // return if the latch were missing.
        for name in ["alpha", "beta"] {
            let sub = dir.path().join(name);
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(format!("{name}.dll")), b"x").unwrap();
        }
        let mut host = PluginHost::default();
        assert_eq!(host.discover(dir.path()).unwrap(), 2);

        let first = host.next_pending_load().expect("one pending");
        assert!(
            host.next_pending_load().is_none(),
            "a nested pass got a second PendingLoad while one was outstanding"
        );
        // Committing a failure releases the latch and does not
        // advance the id base.
        let _ = host.commit_load(&first, Err("not a real dll".to_string()));
        let second = host.next_pending_load().expect("the latch released");
        assert_ne!(second.idx, first.idx, "the failed plugin was offered again");
        assert_eq!(
            second.cmd_id_base, first.cmd_id_base,
            "a failed load must not consume command ids"
        );
    }
}
