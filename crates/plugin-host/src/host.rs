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

use codepp_platform::{has_plugin_extension, DynLib, PLUGIN_EXTENSION};

use crate::dispatch::{NPPN_BUFFERACTIVATED, NPPN_READY, NPPN_TBMODIFICATION};
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
        file_name_of(&self.path)
    }

    /// The name this plugin answers to — its file name through
    /// `codepp_core::shortcuts::module_key` — which is what every
    /// record keyed on a plugin compares by: `shortcuts.xml`, a dock
    /// panel's module, and discovery's refusal of a second plugin
    /// under a name already registered.
    ///
    /// Compare plugins by this and nothing else. Discovery guarantees
    /// one plugin per name only under this normalisation, so a lookup
    /// that normalised differently could reach a plugin discovery
    /// never checked against the rest.
    #[must_use]
    pub fn module_key(&self) -> String {
        module_key_of(&self.path)
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

    /// Enumerate plugin candidates in `dir`. Each becomes a
    /// `PluginInfo` in the `Pending` state — the file is **not** yet
    /// `LoadLibrary`'d. Returns the count discovered.
    ///
    /// A non-existent directory is not an error; it's the first-run
    /// case.
    ///
    /// **The layout is Notepad++'s, and only Notepad++'s:
    /// `plugins/<name>/<name>.<ext>`.** Measured against Notepad++
    /// 8.9.6 with one probe DLL copied into every layout this function
    /// has ever accepted, it loads a plugin only from a folder holding
    /// a file named after that folder, with the case of the two free
    /// to differ. It does not load a DLL placed directly in `plugins/`,
    /// one a level deeper (`plugins/X/x64/X.dll`), one whose name
    /// differs from its folder's, or anything in the `Config` folder,
    /// where plugins keep their settings. Code++ used to accept the
    /// first two as well, and the flat one meant a profile holding an
    /// old `plugins/X.dll` beside the staged `plugins/X/X.dll` loaded
    /// the plugin twice: two submenus, two sets of load-time
    /// notifications, and every record keyed on a plugin's file name —
    /// `disabled.txt`, `shortcuts.xml`, a dock panel's module —
    /// answering to both. A DLL left directly in the folder is logged
    /// rather than loaded, so someone whose plugin has stopped loading
    /// can find out why.
    ///
    /// Looking only at the file named after its folder also keeps a
    /// plugin's own dependencies out of the registry: `ComparePlus`
    /// ships `git2.dll` and `sqlite3.dll` under
    /// `plugins/ComparePlus/libs/`, and loading those as plugins would
    /// run their `DllMain` in the host for nothing.
    ///
    /// **At most one plugin per name.** Everything downstream keys on a
    /// plugin's file name through `codepp_core::shortcuts::module_key`,
    /// so a second plugin answering to a name already registered is
    /// refused, with a warning, and the first keeps it. On Windows the
    /// layout makes that unreachable, because folder names are unique
    /// there regardless of case; but a case-sensitive file system can
    /// hold `Foo/` beside `foo/`, and `module_key` reads `libfoo.so` and
    /// `foo.so` as one name. Folders are visited in a fixed order (see
    /// `sorted_entries`), so which one wins does not depend on how the
    /// file system happens to list a directory. A second call over the
    /// same folder registers nothing new, for the same reason.
    ///
    /// **Links are followed, deliberately.** `is_dir()` and `is_file()`
    /// resolve symlinks and junctions, so a plugin folder — or the file
    /// in it — may be a link to somewhere outside `dir`, and the plugin
    /// it reaches is recorded and later loaded. That is an accepted
    /// risk, not a gap waiting on a check. Placing a link here takes
    /// write access to the plugins folder, which already lets anyone
    /// put a real DLL here, so refusing links would stop nothing an
    /// attacker needs — and it would break the ordinary way to develop
    /// a plugin, which is to link its build output into the folder.
    /// Unprivileged symlink creation on Linux and macOS changes neither
    /// half of that.
    ///
    /// The same access covers the gap between discovery and the load.
    /// Discovery records a path and the load resolves it afresh, which
    /// can be much later — loading is lazy, so a plugin nothing touches
    /// is resolved only when something finally does — and whatever sits
    /// at the path by then is what loads. Swapping it takes the same
    /// write access as replacing the file outright. All of this assumes
    /// the plugins folder belongs to the user running Code++, as
    /// `%APPDATA%\Code++\plugins` and its equivalents do; a folder
    /// writable across a trust boundary would need its own analysis.
    ///
    /// # Errors
    ///
    /// Currently every read-dir failure is absorbed (matching the "no
    /// plugins folder yet" first-run case), so this signature is
    /// `Result` mostly for forward-compat with a future stricter mode.
    /// Today it always returns `Ok`.
    pub fn discover(&mut self, dir: &Path) -> std::io::Result<usize> {
        // No `exists()` pre-check: a separate stat-then-open opens a
        // TOCTOU window where an attacker who can swap `dir` for a
        // symlink between the check and the `read_dir` call could
        // redirect enumeration into a directory of their choosing,
        // with the recorded paths later fed to `LoadLibraryW` at
        // first-touch load. `sorted_entries` already treats a
        // missing-directory `read_dir` failure as "no entries"
        // (matching the first-run case), so the redundant pre-check
        // adds the race without buying anything.
        let mut found = 0usize;
        for path in sorted_entries(dir) {
            if path.is_dir() {
                if is_plugin_config_dir(&path) {
                    continue;
                }
                if let Some(file) = plugin_file_in(&path) {
                    if self.register_discovered(file) {
                        found += 1;
                    }
                }
            } else if path.is_file() && has_plugin_extension(&path) {
                tracing::warn!(
                    path = ?path,
                    "not loading a plugin placed directly in the plugins folder; \
                     Notepad++'s layout, and the only one loaded, is plugins/<name>/<name>"
                );
            }
        }
        Ok(found)
    }

    /// Record `path` as a pending plugin, unless a plugin already
    /// registered answers to the same name — see [`Self::discover`].
    /// Returns whether it was recorded.
    fn register_discovered(&mut self, path: PathBuf) -> bool {
        let key = module_key_of(&path);
        if let Some(first) = self.plugins.iter().find(|p| p.module_key() == key) {
            tracing::warn!(
                path = ?path,
                first = ?first.path,
                "another installed plugin already answers to this name; not loading this one"
            );
            return false;
        }
        self.plugins.push(PluginInfo {
            path,
            name: None,
            state: PluginState::Pending,
            // Default to enabled at discovery time. The shell sweeps
            // `apply_disabled_list` over the registry once
            // enumeration finishes, flipping `disabled = true` for any
            // DLL whose filename appears in `disabled.txt`.
            disabled: false,
        });
        true
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
            let basename = plugin.filename();
            plugin.disabled = disabled_filenames
                .iter()
                .any(|d| filenames_eq(d, &basename));
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
        self.next_pending_load_in(None)
    }

    /// [`Self::next_pending_load`], restricted to a set of module
    /// keys (`codepp_core::shortcuts::module_key` spelling).
    ///
    /// `None` means "any plugin", which is what the lazy triggers in
    /// §6.4 want. A set is what the startup restore wants: a panel
    /// the user had docked last session needs *its* plugin loaded and
    /// nobody else's, so the §8 constraint — no plugin loaded until
    /// something touches it — still holds for the other thirty-nine a
    /// user may have installed.
    pub fn next_pending_load_in(&mut self, only: Option<&[String]>) -> Option<PendingLoad> {
        if self.load_in_progress {
            // A nested loader — see the field doc. Answering here
            // would duplicate a command-id range.
            return None;
        }
        let cmd_id_base = self.next_cmd_id;
        let (idx, plugin) = self.plugins.iter().enumerate().find(|(_, p)| {
            if p.is_loaded() || p.failed_reason().is_some() || p.disabled {
                return false;
            }
            only.is_none_or(|keys| keys.contains(&p.module_key()))
        })?;
        let pending = PendingLoad {
            idx,
            path: plugin.path.clone(),
            cmd_id_base,
        };
        self.load_in_progress = true;
        Some(pending)
    }

    /// Record the outcome of an [`execute_load`], and hand back the
    /// plugin's handle for the notifications the caller must deliver
    /// **after** dropping the host borrow — collected into a
    /// [`LoadNotifications`] with every other plugin the same pass
    /// loads, and delivered once the pass is over.
    ///
    /// They are delivered by the caller rather than here for the same
    /// reason `setInfo` is: a plugin that queries the host from
    /// `NPPN_READY` (its config directory, the version) is doing
    /// something ordinary, and it can only be answered if nothing is
    /// borrowed.
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
                    idx: pending.idx,
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
    /// the plugin's `setInfo`, `getFuncsArray` and its load-time
    /// notifications,
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
        let mut notices = LoadNotifications::default();
        if let Some(ready) = self.commit_load(&pending, loaded)? {
            notices.push(ready);
        }
        // No buffer to announce: a harness owns no documents.
        notices.deliver(npp_data.npp_handle, || None, || {});
        Ok(())
    }

    /// Find the `FuncItem` matching `cmd_id` across all loaded plugins
    /// and return its callback. The callback is a plain C function
    /// pointer; the caller must invoke it from the UI thread (parity
    /// with Notepad++) and may want to wrap the call in
    /// `catch_unwind` to keep panics from unwinding across the FFI.
    #[must_use]
    pub fn lookup_cmd(&self, cmd_id: i32) -> Option<crate::ffi::PluginCmd> {
        self.lookup_command(cmd_id).map(|command| command.func)
    }

    /// [`Self::lookup_cmd`] with the plugin the command belongs to,
    /// which is what a backend runs a menu command through:
    /// [`crate::PluginCommand::run`] marks that plugin as the one
    /// being called while its command runs, so a dock panel the command
    /// registers is known to be its own (see [`crate::caller`]).
    #[must_use]
    pub fn lookup_command(&self, cmd_id: i32) -> Option<crate::PluginCommand> {
        for (owner, plugin) in self.plugins.iter().enumerate() {
            if let Some(funcs) = plugin.func_items() {
                if let Some(f) = funcs.iter().find(|f| f.cmd_id == cmd_id) {
                    return f.p_func.map(|func| crate::PluginCommand { owner, func });
                }
            }
        }
        None
    }

    /// The `messageProc` of the loaded plugin at registry index `idx`,
    /// marked as that plugin when called — see
    /// [`crate::PluginMessageProc::send`]. `None` for an index nothing is
    /// registered at, or a plugin that is not loaded.
    #[must_use]
    pub fn message_target(&self, idx: usize) -> Option<crate::PluginMessageProc> {
        let func = self.plugins.get(idx)?.message_proc_fn()?;
        Some(crate::PluginMessageProc { owner: idx, func })
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
    /// it. `NPPN_SHUTDOWN` still fires while everything is still
    /// mapped — from `WM_CLOSE` on Win32 and from the quit path on
    /// GTK — so a plugin still gets its documented chance to save
    /// state. The Cocoa backend does not send it yet (DESIGN.md §7.4).
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
/// notifications with no host borrow held. Collect one per plugin a
/// load pass commits into a [`LoadNotifications`], which delivers them.
#[derive(Clone, Debug)]
pub struct PluginReady {
    /// Registry index, marked as the calling plugin while its handler
    /// runs — see [`crate::caller`].
    idx: usize,
    /// Only for the log line on a panicking handler.
    path: std::path::PathBuf,
    be_notified: crate::ffi::BeNotifiedFn,
}

impl PluginReady {
    fn notify(&self, npp_handle: crate::ffi::Hwnd, code: u32, id_from: usize) {
        let sci = SCNotification {
            nmhdr: SciNotifyHeader {
                hwnd_from: npp_handle,
                id_from,
                code,
            },
            ..SCNotification::default()
        };
        // A dock panel registered from `NPPN_TBMODIFICATION` — the
        // moment the ABI sets aside for it — is this plugin's.
        let _calling = crate::caller::CallingPlugin::enter(self.idx);
        // SAFETY: `be_notified` came from a successful resolve in
        // `execute_load`, its library is still mapped (plugins are
        // never unloaded before the host drops), and the
        // `SCNotification` is `#[repr(C)]` and lives on this stack
        // frame through the synchronous call.
        let result = catch_unwind(AssertUnwindSafe(|| unsafe {
            (self.be_notified)(&raw const sci);
        }));
        if result.is_err() {
            // Same warn-on-panic posture as `notify_all`: the caller
            // is told the load succeeded either way, so a plugin that
            // dies during its own init would otherwise be invisible.
            tracing::warn!(path = ?self.path, code = code, "plugin panicked in beNotified");
        }
    }
}

/// The load-time notifications owed to every plugin one load pass
/// committed, delivered in Notepad++'s startup order.
///
/// That order was measured rather than assumed: a probe plugin loaded
/// twice into a real Notepad++ 8.9.6 sees every plugin loaded first,
/// then — with every plugin's menu already built — `NPPN_TBMODIFICATION`
/// broadcast to all of them, then `NPPN_BUFFERACTIVATED` for the
/// buffer the session opened on, then `NPPN_READY` to all of them.
/// Code++ used to send each plugin `NPPN_READY` and then
/// `NPPN_TBMODIFICATION`, one plugin at a time, straight after its
/// own load — reversed, and interleaved with the next plugin's load.
///
/// Why the order matters to a plugin: `NPPN_TBMODIFICATION` is where
/// it registers toolbar icons and dock panels, and `NPPN_READY` is
/// where it acts on them — shows a panel it registered, sets the
/// state of a button it added — so READY arriving first means acting
/// on things that do not exist yet. And a plugin handling `NPPN_READY`
/// may message another (`NPPM_MSGTOPLUGIN`), which Notepad++
/// guarantees is loaded by then because READY is a broadcast that
/// follows every load.
///
/// The `NPPN_BUFFERACTIVATED` is Code++'s one deliberate synthesis.
/// Under Notepad++ a plugin always sees the active buffer activated
/// before READY, because the session opens after plugins load; under
/// Code++'s lazy loading that activation happened before the plugin
/// existed. Announcing the buffer that is active *now* gives a plugin
/// the same "I have seen the current buffer" state it would have had,
/// which is what plugins that key per-buffer state on
/// `NPPN_BUFFERACTIVATED` rely on. It goes only to the plugins this
/// pass loaded — the rest already saw that activation.
///
/// Between `NPPN_TBMODIFICATION` and that activation, Notepad++ also
/// brings back the dock panels the last session left open, and it does
/// so by running each one's own menu command — `FuncItem[dlgID]` of the
/// plugin that registered it — rather than by showing a window it
/// already has. Measured the same way: a probe plugin whose panel was
/// open at quit sees its "show panel" command invoked at the next
/// start, after `NPPN_TBMODIFICATION` and before `NPPN_READY`, even
/// when it had registered that panel itself a moment earlier. A plugin
/// that only registers its panel from that command depends on it, and
/// every plugin's menu check and "is my panel open" state is set by it.
/// [`Self::deliver`] leaves the slot to its caller, because what a
/// panel is and how a command is dispatched are the backend's.
///
/// **Deliver with no host borrow held.** A plugin's handler may send
/// `NPPM_*` straight back, and it can only be answered if nothing is
/// borrowed.
#[derive(Clone, Debug, Default)]
pub struct LoadNotifications {
    plugins: Vec<PluginReady>,
}

impl LoadNotifications {
    /// Add one plugin, in load order.
    pub fn push(&mut self, ready: PluginReady) {
        self.plugins.push(ready);
    }

    /// Whether the pass loaded nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Broadcast `NPPN_TBMODIFICATION`, then run `restore_panels`, then
    /// broadcast `NPPN_BUFFERACTIVATED` for the active buffer, then
    /// `NPPN_READY` — each broadcast reaching every plugin in load
    /// order before the next begins. Does nothing, the hook included,
    /// when the pass loaded no plugin.
    ///
    /// `restore_panels` is where the caller brings back the dock panels
    /// these plugins had open — see the type's docs for why it sits
    /// there. It runs with plugin code on both sides of it, so it must
    /// take no borrow the caller holds. A panic in it is contained: it
    /// is host code, and a host bug must not cost every plugin its
    /// `NPPN_READY`, which is what finishes a plugin's initialisation.
    ///
    /// `active_buffer` is asked afresh for each plugin, just before its
    /// `NPPN_BUFFERACTIVATED`, and that plugin's is skipped when it
    /// answers `None`. Not a snapshot, because the handlers run with no
    /// borrow held and may change what is active: a plugin that closes
    /// the active tab from its `NPPN_TBMODIFICATION` would otherwise
    /// have every later plugin told about a buffer that no longer
    /// exists. (The close queues a real `NPPN_BUFFERACTIVATED` for
    /// the tab that replaces it, so each plugin still ends on the truth
    /// either way; this keeps the synthetic one from being false in
    /// between.) The callback must take no borrow the caller holds —
    /// it is called with plugin code before and after it.
    pub fn deliver(
        &self,
        npp_handle: crate::ffi::Hwnd,
        active_buffer: impl Fn() -> Option<usize>,
        restore_panels: impl FnOnce(),
    ) {
        if self.plugins.is_empty() {
            return;
        }
        for ready in &self.plugins {
            ready.notify(npp_handle, NPPN_TBMODIFICATION, 0);
        }
        if catch_unwind(AssertUnwindSafe(restore_panels)).is_err() {
            tracing::warn!("restoring plugin dock panels panicked; continuing to NPPN_READY");
        }
        for ready in &self.plugins {
            if let Some(buffer) = active_buffer() {
                ready.notify(npp_handle, NPPN_BUFFERACTIVATED, buffer);
            }
        }
        for ready in &self.plugins {
            ready.notify(npp_handle, NPPN_READY, 0);
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
    // Whatever `setInfo` or `getFuncsArray` sends the host was sent by
    // this plugin — a panel registered from `setInfo` among it.
    let _calling = crate::caller::CallingPlugin::enter(pending.idx);
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

/// Run a freshly mapped plugin's four init entry points, in
/// Notepad++'s order, and return its name and its raw `FuncItem` array.
///
/// The order — isUnicode, getName, setInfo, getFuncsArray — was
/// measured with a probe plugin loaded into a real Notepad++. Code++
/// used to call setInfo first and never called isUnicode at all. Each
/// FFI call is wrapped in `catch_unwind` so a Rust-authored plugin that
/// panics doesn't unwind across the C ABI (that's UB; DESIGN.md §6.5).
/// C++ plugins that throw past their own ABI are out of scope — broken
/// in Notepad++ too.
///
/// An ANSI plugin is refused before any other entry point runs, which
/// is also what Notepad++ does (it stops at the same call and reports
/// the DLL as failed to load). The host hands plugins UTF-16
/// everywhere — menu labels, paths, every `NPPM_*` string — so an ANSI
/// plugin could only ever misread them.
///
/// `accepted` runs between the isUnicode check and getName — once the
/// plugin is known to be loadable, before any more of it runs.
///
/// A refused plugin's library is freed when its `DynLib` drops, as on
/// every failed load — which is also what Notepad++ does with it. That
/// is the one place the host unloads a plugin; the "never unload"
/// policy (DESIGN.md §6.4) is about a plugin that initialised, whose
/// windows, threads and hooks may outlive a `FreeLibrary`. A plugin
/// refused here has run its `DllMain` and `isUnicode` and nothing else.
fn run_init_entry_points(
    is_unicode: IsUnicodeFn,
    get_name: GetNameFn,
    set_info: SetInfoFn,
    get_funcs_array: GetFuncsArrayFn,
    npp_data: NppData,
    accepted: impl FnOnce(),
) -> Result<(String, *mut FuncItem, i32), String> {
    let unicode = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: is_unicode has the C ABI declared in
        // PluginInterface.h and takes no arguments.
        unsafe { is_unicode() }
    }))
    .map_err(|_| "plugin panicked in isUnicode".to_string())?;
    if unicode == 0 {
        return Err(
            "isUnicode returned FALSE: an ANSI plugin, which a Unicode host cannot load"
                .to_string(),
        );
    }
    accepted();

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

    // setInfo — the plugin stashes the host handles before it is
    // asked for its menu items.
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: set_info has the C ABI declared in
        // PluginInterface.h; npp_data is a valid #[repr(C)] NppData
        // by construction.
        unsafe { set_info(npp_data) }
    }))
    .map_err(|_| "plugin panicked in setInfo".to_string())?;

    // getFuncsArray — plugin returns a pointer to its menu items.
    let mut count: i32 = 0;
    let raw = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: get_funcs_array signature declared in ffi; count
        // is a valid out-pointer.
        unsafe { get_funcs_array(&raw mut count) }
    }))
    .map_err(|_| "plugin panicked in getFuncsArray".to_string())?;
    Ok((name, raw, count))
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

    // The host's message-routing callback goes in once the plugin has
    // passed isUnicode, and before anything else of it runs, so its
    // `SendMessage` transport is live for every message it could send
    // (NPPN_READY's beNotified, menu commands) — and a plugin refused
    // as ANSI is never handed a live route into the host at all.
    let (name, raw, count) = run_init_entry_points(
        is_unicode,
        get_name,
        set_info,
        get_funcs_array,
        npp_data,
        || install_dispatch(&lib, dispatch, path),
    )?;
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

/// The entries of `dir`, in a fixed order: by upper-cased file name,
/// ties broken by the name itself.
///
/// NTFS already enumerates a directory in that order (for ASCII
/// names), so on Windows this is the order `read_dir` gives and the
/// order plugins have always loaded in; elsewhere it replaces an order
/// that belongs to the file system — hash order on ext4 — with one
/// that does not. It matters because the first plugin to claim a name
/// keeps it ([`PluginHost::discover`]), and load order is also
/// command-id order and Plugins-menu order.
///
/// An unreadable or missing directory has no entries: the first-run
/// case of a plugins folder that does not exist yet.
fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    order_entries(&mut paths);
    paths
}

/// Sort `paths` into [`sorted_entries`] order. Apart from reading the
/// directory so it can be tested on any file system — NTFS would
/// hand a test the right order whether or not this ran.
///
/// Upper-cased with Unicode's mapping, not the ASCII-only folding
/// identity uses (`module_key`), on purpose: this imitates NTFS, whose
/// upcase table covers far more than ASCII, so Windows keeps the order
/// it always had for a non-ASCII name too. The two cannot disagree on
/// anything identity decides — names that are one plugin under
/// `module_key` differ only in ASCII case or a `lib` prefix, and sort
/// the same way under either mapping — so this only orders plugins
/// that are different anyway.
fn order_entries(paths: &mut [PathBuf]) {
    paths.sort_by_cached_key(|p| {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        (name.to_uppercase(), name)
    });
}

/// Whether `path` is the plugins folder's `config` directory, which
/// is where plugins keep their settings (`NPPM_GETPLUGINSCONFIGDIR`
/// points there) and never a plugin, whatever it holds. Notepad++
/// skips its `Config` folder by name — measured: a
/// `plugins/Config/Config.dll` is not loaded.
fn is_plugin_config_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("config"))
}

/// The plugin in plugin folder `dir`: the file named after the
/// folder, `<name>.<ext>`, or `None` if there is none.
///
/// Notepad++ builds that path from the folder name and lets the file
/// system resolve it, so the case of the file's name is free to differ
/// from the folder's; this does the same, and so returns the path
/// spelled the way the folder is. A case-sensitive file system does
/// not resolve it that way, so there the folder is searched for a file
/// whose name matches ignoring ASCII case, and the first in
/// [`sorted_entries`] order is taken.
fn plugin_file_in(dir: &Path) -> Option<PathBuf> {
    let name = dir.file_name()?.to_str()?;
    let exact = dir.join(format!("{name}.{PLUGIN_EXTENSION}"));
    if exact.is_file() {
        return Some(exact);
    }
    sorted_entries(dir).into_iter().find(|p| {
        p.is_file()
            && has_plugin_extension(p)
            && p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case(name))
    })
}

/// `path`'s final component as text, or empty if it has none. Lossy,
/// so a name that is not valid Unicode still has a stable spelling.
/// The single conversion behind [`PluginInfo::filename`] and
/// [`module_key_of`], which is what keeps a plugin's recorded name and
/// its identity from ever disagreeing.
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A plugin file's identity, the one every record keyed on a plugin's
/// file name compares by — see [`PluginInfo::module_key`].
fn module_key_of(path: &Path) -> String {
    codepp_core::shortcuts::module_key(&file_name_of(path))
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

// Windows-only: these load real (or deliberately broken) `.dll`s and
// use `.dll`-named fixtures. Discovery, which is platform-neutral file
// logic, has its own module below that runs everywhere. The Linux load
// path (including the dispatch handshake) is covered end-to-end by the
// GTK plugin demo, and `has_plugin_extension` is tested per-OS in
// `codepp-platform`.
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
    fn load_invalid_dll_marks_failed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("not-a-dll")).unwrap();
        std::fs::write(
            dir.path().join("not-a-dll").join("not-a-dll.dll"),
            b"this isn't a real dll",
        )
        .unwrap();
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

/// Discovery, on every platform. It is file-system logic shared by all
/// three backends, so the fixtures use each platform's own plugin
/// extension rather than the `.dll` names the Windows-only module
/// above is written with.
#[cfg(test)]
mod discovery_tests {
    use super::*;

    /// `name.<ext>` with this platform's plugin extension.
    fn plugin_file(name: &str) -> String {
        format!("{name}.{PLUGIN_EXTENSION}")
    }

    /// An empty file at `dir/rel`, creating the folders on the way.
    /// Discovery records paths without mapping anything, so an empty
    /// file is a complete "installed, not yet loaded" plugin.
    fn touch(dir: &Path, rel: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("a parent")).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    /// What `host` discovered under `dir`, relative and `/`-separated,
    /// in registry order.
    fn found(host: &PluginHost, dir: &Path) -> Vec<String> {
        host.iter()
            .map(|p| {
                p.path
                    .strip_prefix(dir)
                    .expect("under the plugins folder")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn a_missing_folder_holds_no_plugins() {
        let mut host = PluginHost::new();
        let n = host
            .discover(&PathBuf::from("definitely-not-a-real-plugin-dir-12345"))
            .unwrap();
        assert_eq!(n, 0);
        assert!(host.is_empty());
    }

    #[test]
    fn an_empty_folder_holds_no_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = PluginHost::new();
        assert_eq!(host.discover(dir.path()).unwrap(), 0);
        assert!(host.is_empty());
    }

    /// Notepad++'s layout: a folder holding a file named after it.
    /// Nothing else in the folder, or beside it, is a plugin.
    #[test]
    fn a_plugin_is_the_file_named_after_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), &format!("alpha/{}", plugin_file("alpha")));
        touch(dir.path(), &format!("beta/{}", plugin_file("beta")));
        touch(dir.path(), "alpha/readme.txt");
        touch(dir.path(), "notes.md");
        let mut host = PluginHost::new();
        assert_eq!(host.discover(dir.path()).unwrap(), 2);
        assert_eq!(
            found(&host, dir.path()),
            [
                format!("alpha/{}", plugin_file("alpha")),
                format!("beta/{}", plugin_file("beta")),
            ]
        );
    }

    /// Each layout Code++ used to accept beside Notepad++'s, plus the
    /// one it never did. Measured against Notepad++ 8.9.6 with one
    /// probe DLL copied into all of them: it loads none.
    #[test]
    fn layouts_notepad_plus_plus_does_not_load_are_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        // Directly in the plugins folder.
        touch(dir.path(), &plugin_file("flat"));
        // A level below the plugin's own folder.
        touch(dir.path(), &format!("arch/x64/{}", plugin_file("arch")));
        // Named differently from its folder.
        touch(dir.path(), &format!("mismatch/{}", plugin_file("other")));
        // Deeper still.
        touch(dir.path(), &format!("a/b/c/{}", plugin_file("c")));
        let mut host = PluginHost::new();
        assert_eq!(host.discover(dir.path()).unwrap(), 0);
        assert!(host.is_empty());
    }

    /// The profile this was found on: an old copy of a plugin left
    /// directly in the plugins folder, beside the staged one. It used
    /// to load as a second plugin; now only the staged one does.
    #[test]
    fn a_flat_copy_beside_the_staged_plugin_is_not_a_second_plugin() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), &plugin_file("hello"));
        touch(dir.path(), &format!("hello/{}", plugin_file("hello")));
        let mut host = PluginHost::new();
        assert_eq!(host.discover(dir.path()).unwrap(), 1);
        assert_eq!(
            found(&host, dir.path()),
            [format!("hello/{}", plugin_file("hello"))]
        );
    }

    /// `ComparePlus` ships its own libraries under `libs/`. Loading
    /// them as plugins would run their `DllMain` in the host for
    /// nothing, and nothing below a plugin's folder is looked at.
    #[test]
    fn a_plugins_own_dependencies_are_not_plugins() {
        let dir = tempfile::tempdir().unwrap();
        touch(
            dir.path(),
            &format!("ComparePlus/{}", plugin_file("ComparePlus")),
        );
        touch(
            dir.path(),
            &format!("ComparePlus/libs/{}", plugin_file("git2")),
        );
        touch(
            dir.path(),
            &format!("ComparePlus/libs/{}", plugin_file("sqlite3")),
        );
        let mut host = PluginHost::new();
        assert_eq!(host.discover(dir.path()).unwrap(), 1);
        assert_eq!(
            found(&host, dir.path()),
            [format!("ComparePlus/{}", plugin_file("ComparePlus"))]
        );
    }

    /// Notepad++ lets the file system resolve the name it builds from
    /// the folder, so the case of the two may differ.
    #[test]
    fn the_file_may_differ_from_its_folder_in_case() {
        let dir = tempfile::tempdir().unwrap();
        touch(
            dir.path(),
            &format!("ComparePlus/{}", plugin_file("compareplus")),
        );
        let mut host = PluginHost::new();
        assert_eq!(host.discover(dir.path()).unwrap(), 1);
    }

    /// The folder plugins keep their settings in is never a plugin,
    /// whatever it holds — Notepad++ skips its `Config` folder by
    /// name (measured).
    #[test]
    fn the_config_folder_is_never_a_plugin() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), &format!("Config/{}", plugin_file("Config")));
        touch(dir.path(), &format!("real/{}", plugin_file("real")));
        let mut host = PluginHost::new();
        assert_eq!(host.discover(dir.path()).unwrap(), 1);
        assert_eq!(
            found(&host, dir.path()),
            [format!("real/{}", plugin_file("real"))]
        );
    }

    /// The order itself, on a list the file system never touched: by
    /// upper-cased name — `_` sorts after letters, as NTFS has it — with
    /// the name itself breaking a tie.
    #[test]
    fn entries_are_ordered_by_upper_cased_name() {
        let mut paths: Vec<PathBuf> = ["zeta", "a_b", "Ab", "ab", "Mid"]
            .iter()
            .map(PathBuf::from)
            .collect();
        order_entries(&mut paths);
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Ab", "ab", "a_b", "Mid", "zeta"]);
    }

    /// Folders are registered by upper-cased name whatever order the
    /// file system lists them in — the order NTFS already uses, and
    /// the one that decides which plugin keeps a contested name.
    #[test]
    fn plugins_are_registered_in_a_fixed_order() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["gamma", "Alpha", "beta"] {
            touch(dir.path(), &format!("{name}/{}", plugin_file(name)));
        }
        let mut host = PluginHost::new();
        host.discover(dir.path()).unwrap();
        assert_eq!(
            found(&host, dir.path()),
            [
                format!("Alpha/{}", plugin_file("Alpha")),
                format!("beta/{}", plugin_file("beta")),
                format!("gamma/{}", plugin_file("gamma")),
            ]
        );
    }

    /// One plugin per name. Everything downstream keys on a plugin's
    /// file name through `module_key`, so a second path answering to
    /// a name already registered is refused, whatever folder it is in.
    #[test]
    fn a_second_plugin_answering_to_a_registered_name_is_refused() {
        let mut host = PluginHost::new();
        assert!(host.register_discovered(PathBuf::from("one").join(plugin_file("Foo"))));
        assert!(!host.register_discovered(PathBuf::from("two").join(plugin_file("foo"))));
        assert!(host.register_discovered(PathBuf::from("three").join(plugin_file("bar"))));
        assert_eq!(host.len(), 2);
        assert_eq!(
            host.iter().next().expect("first").path,
            PathBuf::from("one").join(plugin_file("Foo")),
            "the first plugin to claim a name keeps it"
        );
    }

    /// A second discovery of the same folder finds every plugin
    /// already registered and adds none.
    #[test]
    fn discovering_a_folder_twice_registers_nothing_new() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), &format!("alpha/{}", plugin_file("alpha")));
        let mut host = PluginHost::new();
        assert_eq!(host.discover(dir.path()).unwrap(), 1);
        assert_eq!(host.discover(dir.path()).unwrap(), 0);
        assert_eq!(host.len(), 1);
    }

    /// `module_key` reads `libfoo.so` and `foo.so` as one name — it
    /// undoes Cargo's `cdylib` prefix — so a Unix plugins folder can
    /// hold two plugins answering to one name in the right layout.
    #[cfg(unix)]
    #[test]
    fn a_lib_prefixed_twin_is_not_a_second_plugin() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), &format!("foo/{}", plugin_file("foo")));
        touch(dir.path(), &format!("libfoo/{}", plugin_file("libfoo")));
        let mut host = PluginHost::new();
        assert_eq!(host.discover(dir.path()).unwrap(), 1);
        assert_eq!(
            found(&host, dir.path()),
            [format!("foo/{}", plugin_file("foo"))]
        );
    }

    /// A case-sensitive file system can hold two folders whose names
    /// differ only in case; they answer to one name.
    #[cfg(target_os = "linux")]
    #[test]
    fn folders_differing_only_in_case_are_one_plugin() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), &format!("Foo/{}", plugin_file("Foo")));
        touch(dir.path(), &format!("foo/{}", plugin_file("foo")));
        let mut host = PluginHost::new();
        assert_eq!(host.discover(dir.path()).unwrap(), 1);
        assert_eq!(
            found(&host, dir.path()),
            [format!("Foo/{}", plugin_file("Foo"))]
        );
    }

    /// A plugin's recorded name and its identity come from one
    /// conversion of its file name, so they cannot disagree — not even
    /// for a name that is not valid Unicode, which only a Unix file
    /// system can hold. Discovery keys a plugin by `module_key`, and
    /// every lookup keyed on its file name must reach the same answer:
    /// one that dropped such a name to "" instead would stop matching
    /// the plugin discovery registered.
    #[cfg(unix)]
    #[test]
    fn a_plugins_file_name_and_identity_come_from_one_conversion() {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9.so");
        let plugin = PluginInfo {
            path: Path::new("/plugins/caf").join(name),
            name: None,
            state: PluginState::Pending,
            disabled: false,
        };
        assert!(!plugin.filename().is_empty(), "a lossy spelling, not none");
        assert_eq!(
            plugin.module_key(),
            codepp_core::shortcuts::module_key(&plugin.filename())
        );
    }

    #[test]
    fn a_pending_plugin_is_labelled_by_its_file_stem() {
        let dir = tempfile::tempdir().unwrap();
        touch(
            dir.path(),
            &format!("my-plugin/{}", plugin_file("my-plugin")),
        );
        let mut host = PluginHost::new();
        host.discover(dir.path()).unwrap();
        assert_eq!(host.iter().next().unwrap().display_label(), "my-plugin");
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

/// The load-time lifecycle order, on every platform: it is shared by
/// all three backends, so a test that ran only on Windows would leave
/// the GTK and Cocoa order unguarded.
#[cfg(test)]
mod load_order_tests {
    use super::*;

    use std::sync::Mutex;

    /// `(plugin, code, id_from)` in the order the recorders saw them.
    static SEEN: Mutex<Vec<(char, u32, usize)>> = Mutex::new(Vec::new());

    fn record(who: char, sci: *const SCNotification) {
        // SAFETY: `LoadNotifications` hands a live `SCNotification`.
        let h = unsafe { &(*sci).nmhdr };
        SEEN.lock()
            .expect("recorder lock")
            .push((who, h.code, h.id_from));
    }
    unsafe extern "C" fn plugin_a(sci: *const SCNotification) {
        record('a', sci);
    }
    unsafe extern "C" fn plugin_b(sci: *const SCNotification) {
        record('b', sci);
    }

    fn ready(idx: usize, be_notified: crate::ffi::BeNotifiedFn) -> PluginReady {
        PluginReady {
            idx,
            path: PathBuf::from("recorder"),
            be_notified,
        }
    }

    /// Notepad++'s startup order, as measured: each notification is a
    /// broadcast that reaches every plugin before the next one starts,
    /// and TBMODIFICATION comes first — the reverse of what Code++
    /// used to send, one plugin at a time.
    ///
    /// Both cases run in one test because the recorder is a process
    /// global and libtest runs tests in parallel.
    #[test]
    fn load_notifications_broadcast_in_notepad_plus_plus_order() {
        let mut notices = LoadNotifications::default();
        assert!(notices.is_empty());
        notices.push(ready(0, plugin_a));
        notices.push(ready(1, plugin_b));

        // `'*'` marks the panel-restore slot, which Notepad++ runs
        // after every plugin has had NPPN_TBMODIFICATION — where panels
        // are registered — and before any hears NPPN_READY.
        SEEN.lock().expect("recorder lock").clear();
        notices.deliver(
            core::ptr::null_mut(),
            || Some(7),
            || SEEN.lock().expect("recorder lock").push(('*', 0, 0)),
        );
        assert_eq!(
            *SEEN.lock().expect("recorder lock"),
            vec![
                ('a', NPPN_TBMODIFICATION, 0),
                ('b', NPPN_TBMODIFICATION, 0),
                ('*', 0, 0),
                ('a', NPPN_BUFFERACTIVATED, 7),
                ('b', NPPN_BUFFERACTIVATED, 7),
                ('a', NPPN_READY, 0),
                ('b', NPPN_READY, 0),
            ]
        );

        // No buffer to announce: the activation is skipped, the rest
        // keeps its order.
        SEEN.lock().expect("recorder lock").clear();
        notices.deliver(core::ptr::null_mut(), || None, || {});
        assert_eq!(
            *SEEN.lock().expect("recorder lock"),
            vec![
                ('a', NPPN_TBMODIFICATION, 0),
                ('b', NPPN_TBMODIFICATION, 0),
                ('a', NPPN_READY, 0),
                ('b', NPPN_READY, 0),
            ]
        );

        // The active buffer is asked for per plugin, at delivery: a
        // handler that changes it (closing the tab, say) must not leave
        // the plugins after it announced a buffer that is gone.
        SEEN.lock().expect("recorder lock").clear();
        let asked = std::cell::Cell::new(0usize);
        notices.deliver(
            core::ptr::null_mut(),
            || {
                asked.set(asked.get() + 1);
                Some(10 + asked.get())
            },
            || {},
        );
        let activations: Vec<(char, usize)> = SEEN
            .lock()
            .expect("recorder lock")
            .iter()
            .filter(|(_, code, _)| *code == NPPN_BUFFERACTIVATED)
            .map(|(who, _, id)| (*who, *id))
            .collect();
        assert_eq!(activations, vec![('a', 11), ('b', 12)]);

        // A restore that panics is host code failing, and must not
        // cost the plugins the NPPN_READY that finishes their init.
        SEEN.lock().expect("recorder lock").clear();
        notices.deliver(core::ptr::null_mut(), || None, || panic!("restore failed"));
        assert_eq!(
            *SEEN.lock().expect("recorder lock"),
            vec![
                ('a', NPPN_TBMODIFICATION, 0),
                ('b', NPPN_TBMODIFICATION, 0),
                ('a', NPPN_READY, 0),
                ('b', NPPN_READY, 0),
            ]
        );

        // A pass that loaded nothing restores nothing.
        let ran = std::cell::Cell::new(false);
        LoadNotifications::default().deliver(core::ptr::null_mut(), || Some(1), || ran.set(true));
        assert!(!ran.get(), "an empty pass ran the panel restore");
    }

    std::thread_local! {
        /// The plugin each load-time handler, and the restore slot, ran as.
        static RAN_AS: std::cell::RefCell<Vec<(char, Option<usize>)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    unsafe extern "C" fn record_calling_plugin(_sci: *const SCNotification) {
        RAN_AS.with(|r| r.borrow_mut().push(('p', crate::calling_plugin())));
    }

    /// Each plugin's load-time handlers run marked as that plugin — a
    /// panel registered from `NPPN_TBMODIFICATION` is known to be its
    /// own — while the restore slot between them, which is host code,
    /// runs with no plugin marked: the commands it sends mark their own.
    #[test]
    fn load_notifications_run_each_handler_as_its_plugin() {
        let mut notices = LoadNotifications::default();
        notices.push(ready(4, record_calling_plugin));
        notices.push(ready(6, record_calling_plugin));
        RAN_AS.with(|r| r.borrow_mut().clear());
        notices.deliver(
            core::ptr::null_mut(),
            || None,
            || RAN_AS.with(|r| r.borrow_mut().push(('*', crate::calling_plugin()))),
        );
        assert_eq!(
            RAN_AS.with(|r| r.borrow().clone()),
            vec![
                ('p', Some(4)),
                ('p', Some(6)),
                ('*', None),
                ('p', Some(4)),
                ('p', Some(6)),
            ]
        );
    }

    /// A plugin's `setInfo` and `getFuncsArray` run marked as the plugin
    /// being loaded, so a dock panel registered from `setInfo` is known
    /// to be its own. A source check, because observing the mark needs
    /// a real DLL that reports it.
    #[test]
    fn a_plugin_loads_as_itself() {
        // LF only, because the end of the body is found by `"\n}\n"`.
        // `.gitattributes` checks `*.rs` out as LF, but a working copy
        // checked out before the rule arrived keeps its CRLF files until
        // they change.
        let src = include_str!("host.rs").replace("\r\n", "\n");
        let body = &src[src.find("pub fn execute_load(").expect("execute_load")..];
        let body = &body[..body.find("\n}\n").expect("end of execute_load")];
        let mark = body
            .find("CallingPlugin::enter(pending.idx)")
            .expect("the load no longer runs marked as the plugin being loaded");
        let load = body.find("load_inner(").expect("the load itself");
        assert!(
            mark < load,
            "the mark must be set before the plugin's code runs"
        );
    }

    /// A plugin's four init entry points are called in
    /// Notepad++'s order, refusing an ANSI plugin before any of the
    /// others run. A source scan, because observing the order needs a
    /// real DLL that records it — the probe the order was measured
    /// with lives outside the tree.
    #[test]
    fn init_entry_points_are_called_in_notepad_plus_plus_order() {
        let src = include_str!("host.rs");
        let body = &src[src
            .find("fn run_init_entry_points(")
            .expect("run_init_entry_points")..];
        let body = &body[..body.find("\nfn ").unwrap_or(body.len())];
        // Code only, so a call named in a comment cannot stand in for
        // the call itself.
        let body = body
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let at = |call: &str| {
            body.find(call)
                .unwrap_or_else(|| panic!("run_init_entry_points no longer calls {call}"))
        };
        let (unicode, name, info, funcs) = (
            at("unsafe { is_unicode() }"),
            at("let p = get_name();"),
            at("unsafe { set_info(npp_data) }"),
            at("unsafe { get_funcs_array(&raw mut count) }"),
        );
        assert!(
            unicode < name && name < info && info < funcs,
            "the entry points are no longer called isUnicode, getName, setInfo, getFuncsArray"
        );
        let refusal = at("if unicode == 0 {");
        let routed = at("accepted();");
        assert!(
            unicode < refusal && refusal < routed && routed < name,
            "an ANSI plugin must be refused before any other entry point runs, \
             and before it is given a route into the host"
        );
    }
}
