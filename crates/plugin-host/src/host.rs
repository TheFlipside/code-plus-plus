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

use std::mem::ManuallyDrop;
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

    /// Whether a load pass would still load this plugin: it is neither
    /// loaded, failed nor disabled.
    fn awaits_load(&self) -> bool {
        !self.is_loaded() && self.failed_reason().is_none() && !self.disabled
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

/// State of a successfully loaded plugin: the resolved entry-point
/// function pointers and the cached `FuncItem` array.
///
/// It holds no handle to the library. A library is never unmapped
/// once it is mapped ([`keep_mapped`]), so the entry points stay valid
/// for the rest of the process, and dropping a `LoadedPlugin` unmaps
/// nothing: not when the host drops it, not when
/// [`PluginHost::commit_load`] refuses it, and not when a backend loses
/// it on the way there.
///
/// Public only because it travels from [`execute_load`] to
/// [`PluginHost::commit_load`] through the caller, which is what lets
/// the load itself run with no host borrow held — its fields stay
/// private.
pub struct LoadedPlugin {
    /// The load this is the outcome of, so [`PluginHost::commit_load`]
    /// files it under that load's plugin and no other.
    load: PendingLoad,
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

/// First menu-command id assigned to a plugin's `FuncItem`.
///
/// A plugin can name any command id in `NPPM_MENUCOMMAND`,
/// `NPPM_SETMENUITEMCHECK` and the shortcut messages, so plugin
/// commands share one number space with Code++'s own commands and
/// with Notepad++'s. This range and the allocation pool after it
/// clear both. Code++'s own command ids stay below 5000. Notepad++'s
/// menu commands count from its `IDM`, 40000, and end at 49001, and
/// above those it numbers two blocks of its own, each with room to
/// grow: its auto-completion and tab-switching commands, 50000 to
/// 50020 (`IDCMD` to `IDCMD_LIMIT` in its `resource.h`), and its
/// Scintilla key commands, 60000 to 60030 (`IDSCINTILLA` to
/// `IDSCINTILLA_LIMIT`). The range starts a thousand above the first
/// block and ends where the second begins.
///
/// Until Phase 5 it began at 50000, so `NPPM_MENUCOMMAND` with
/// Notepad++'s function completion ran the first loaded plugin's
/// first command (DESIGN.md §7.4). No file Code++ writes holds these
/// numbers: `shortcuts.xml` and `session.xml` name a plugin command
/// by its module and its `FuncItem` index.
pub const PLUGIN_CMD_ID_BASE: i32 = 51_000;

/// First id past the `FuncItem` range: 9000 ids, for every plugin a
/// session loads. A load whose items would not fit fails
/// ([`execute_load`]), so the range never reaches Notepad++'s
/// Scintilla key commands at 60000 or the allocation pool.
pub const PLUGIN_CMD_ID_LIMIT: i32 = 60_000;

/// First plugin-allocatable command id. Drives `NPPM_ALLOCATECMDID`.
/// A thousand above Notepad++'s Scintilla key commands (see
/// [`PLUGIN_CMD_ID_BASE`]), and apart from the `FuncItem` range so a
/// plugin's menu commands and its allocated ids cannot collide.
pub const PLUGIN_ALLOC_CMD_BASE: i32 = 61_000;

/// First id past the plugin-allocatable command range: 4500 ids,
/// comfortably above any plausible plugin's needs, and below
/// `u16::MAX`, since a Win32 `WM_COMMAND` carries its id in 16 bits.
pub const PLUGIN_ALLOC_CMD_LIMIT: i32 = 65_500;

/// Whether `id` is a plugin command id: one a plugin's `FuncItem` was
/// given, or one `NPPM_ALLOCATECMDID` can hand out. Every other id is
/// Code++'s or Notepad++'s, which a host must not answer as a
/// plugin's. A plugin may name any number in a message, so this is
/// how a host tells "a plugin's command" from "a different command
/// that a plugin named" before acting on either.
#[must_use]
pub fn is_plugin_cmd_id(id: i32) -> bool {
    (PLUGIN_CMD_ID_BASE..PLUGIN_CMD_ID_LIMIT).contains(&id)
        || (PLUGIN_ALLOC_CMD_BASE..PLUGIN_ALLOC_CMD_LIMIT).contains(&id)
}

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
    /// and not when it drops either: a plugin's library is given up,
    /// still mapped, as soon as it is mapped (see [`keep_mapped`]).
    /// The UI relies on the first half of that:
    /// `crate::dispatch::NotifyTargets` snapshots the loaded plugins'
    /// `beNotified` pointers and calls them after the borrow on this
    /// host has been dropped. A future unload / hot-reload path must
    /// invalidate those snapshots first.
    plugins: Vec<PluginInfo>,
    /// Next menu-command id to hand out at the next successful load.
    /// Monotonically increasing; never reused so that a plugin which
    /// fails to load cannot leak its allocated cmds onto a later
    /// plugin's items.
    next_cmd_id: i32,
    /// The load in progress, as it was handed out: set by
    /// [`PluginHost::next_pending_load`] or
    /// [`PluginHost::load_blocking`], and cleared by that load's
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
    ///
    /// It holds the whole load rather than only recording that one is
    /// in progress, so `commit_load` takes that load, exactly as it was
    /// handed out, and no other, once: a `PendingLoad` committed
    /// already, one this host did not hand out, or one changed since,
    /// in its plugin, path or base, is refused, changes nothing, and
    /// cannot release the latch of a load still running.
    load_in_progress: Option<PendingLoad>,
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
            load_in_progress: None,
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
        if self.load_in_progress.is_some() {
            // A nested loader — see the field doc. Answering here
            // would duplicate a command-id range.
            return None;
        }
        let idx = self.plugins.iter().position(|p| {
            p.awaits_load() && only.is_none_or(|keys| keys.contains(&p.module_key()))
        })?;
        Some(self.hand_out(idx))
    }

    /// Whether a load pass started now would load a plugin: one is
    /// neither loaded, failed nor disabled, and no load is in progress.
    /// That is exactly when [`Self::next_pending_load`] would hand one out.
    ///
    /// Read-only, unlike `next_pending_load`, which takes the load latch:
    /// a UI asks this to decide whether a lazy trigger has a pass to
    /// start. While a load is in progress it answers `false`, since a
    /// pass started then would load nothing; the UI running that load
    /// knows it is running. So does a latch a load left set without
    /// committing, which no later pass gets past.
    #[must_use]
    pub fn has_load_to_start(&self) -> bool {
        self.load_in_progress.is_none() && self.plugins.iter().any(PluginInfo::awaits_load)
    }

    /// Hand out the load of plugin `idx`, numbered from the next free
    /// command id, and take the latch for it. The one place this host
    /// makes a [`PendingLoad`], so [`Self::commit_load`]'s check that
    /// it commits the load in progress, as it was handed out, holds for
    /// every caller. `idx` must name a discovered plugin.
    fn hand_out(&mut self, idx: usize) -> PendingLoad {
        debug_assert!(
            self.load_in_progress.is_none(),
            "a load was handed out while another was in progress"
        );
        let pending = PendingLoad {
            idx,
            path: self.plugins[idx].path.clone(),
            cmd_id_base: self.next_cmd_id,
        };
        self.load_in_progress = Some(pending.clone());
        pending
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
    /// and surfaced to the UI), `pending` is not the load in progress
    /// (committed already, or not handed out yet), or `result` is the
    /// outcome of another load. Such a commit changes nothing, and the
    /// load in progress keeps the latch. A load refused here leaves its
    /// library mapped, as every library stays once it is mapped
    /// ([`keep_mapped`]).
    ///
    /// Only an `Ok` outcome carries the load it came from. An `Err`
    /// committed for the load in progress is taken as that load's
    /// failure whatever its text: it can mark only the plugin the latch
    /// names as failed, and that plugin is not offered again.
    pub fn commit_load(
        &mut self,
        pending: &PendingLoad,
        result: Result<LoadedPlugin, String>,
    ) -> Result<Option<PluginReady>, String> {
        // Only the load in progress is committed, and only once; its
        // commit releases the latch whatever the outcome. Anything else
        // is refused before it touches the latch or the plugin, so it
        // cannot release a load still running. A `PendingLoad` cannot be
        // built outside this crate, so one from a caller is a load this
        // host handed out: the one in progress, or one committed
        // already, which would record its outcome over the plugin's.
        // Nor may the outcome be another load's, which would file that
        // library under this plugin's entry. A refused outcome is simply
        // dropped: a `LoadedPlugin` holds no handle that could unmap its
        // library.
        // A refusal is a backend's bug rather than a plugin's: logged at
        // `error` here, where the caller logs a load that failed at
        // `warn`, since it leaves the latch held and lazy loading off.
        if self.load_in_progress.as_ref() != Some(pending) {
            let e = format!(
                "PendingLoad for {} is not the load in progress",
                pending.path.display()
            );
            tracing::error!(error = ?e, "plugin load commit refused");
            return Err(e);
        }
        if result.as_ref().is_ok_and(|loaded| loaded.load != *pending) {
            let e = format!(
                "the outcome committed for {} is another load's",
                pending.path.display()
            );
            tracing::error!(error = ?e, "plugin load commit refused");
            return Err(e);
        }
        self.load_in_progress = None;
        // In range: the load was handed out for a discovered plugin, and
        // entries are never removed.
        let plugin = &mut self.plugins[pending.idx];
        // What matching the load as handed out guarantees: a plugin's
        // path never changes once it is discovered, and only a commit
        // moves the next free id, which no other load could reach while
        // this one held the latch.
        debug_assert_eq!(plugin.path, pending.path);
        debug_assert_eq!(pending.cmd_id_base, self.next_cmd_id);
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
    /// **Not for a UI backend.** This holds the host borrow across the
    /// plugin's `setInfo`, `getFuncsArray` and its load-time
    /// notifications, which is precisely what the three-phase API
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
    /// It takes the load latch as [`Self::next_pending_load`] does, and
    /// like it does not load a plugin that is loaded already or has
    /// failed, since a second try could run the plugin's init twice. A
    /// plugin loaded already, or disabled, is left alone with `Ok(())`,
    /// and no latch is taken for it. A panic in the load itself, a host
    /// bug, propagates and leaves the latch set; the backends, which
    /// must survive one, run [`execute_load`] inside `catch_unwind`.
    ///
    /// # Errors
    ///
    /// `idx` names no plugin, the plugin failed to load before (its
    /// recorded reason), another load is in progress, the library
    /// failed to map, an entry point was missing, or the plugin
    /// published a malformed `FuncItem` array.
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
        if let Some(reason) = plugin.failed_reason() {
            return Err(reason.to_string());
        }
        if self.load_in_progress.is_some() {
            return Err("another plugin's load is in progress".to_string());
        }
        let pending = self.hand_out(idx);
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

/// Wrap a plugin's library the moment it is mapped, so that no handle
/// to it ever unmaps it: the library stays mapped for the rest of the
/// process.
///
/// `load_inner` wraps every library as soon as `DynLib::load` returns,
/// before any of its entry points is looked up, so no later step can
/// unmap it: not a refusal before `setInfo` (an ANSI plugin, a missing
/// entry point), not one after it, not a load
/// [`PluginHost::commit_load`] refuses or a backend never commits, and
/// not the host's own drop. Notepad++ unmaps a plugin it refuses before
/// `setInfo`; Code++ keeps it, at the cost of one mapping for each
/// refused plugin, which is never tried again. On Windows a mapped DLL
/// cannot be deleted or replaced, so a refused plugin's file stays
/// locked until Code++ exits, as a loaded plugin's always has: updating
/// such a plugin takes a restart.
///
/// **Unmapping a plugin that has run any of its code can crash the
/// host.** Loading a library runs its `DllMain` (a shared object's
/// constructors), and a refused plugin has run `isUnicode` as well;
/// either may have started a thread, a hook or a timer. Measured on
/// Linux: a test plugin that starts a thread in `isUnicode` and returns
/// FALSE crashed the host with `SIGSEGV` when the thread next ran, once
/// the refusal had unmapped its library. A plugin that has run its init
/// leaves more behind. `DynLib`'s own `Drop` calls `FreeLibrary`, and
/// running it at process teardown crashed Code++. Measured rather than
/// reasoned about: with the real `NppExec` loaded, Code++ exited
/// `0xC0000005` (`STATUS_ACCESS_VIOLATION`) every time, and skipping
/// the `FreeLibrary` made it exit `0` every time, with no other change.
///
/// The reason is that a host cannot know what a plugin still has
/// alive. Unmapping the library leaves any window whose `WNDPROC`
/// lives in it, any `SetTimer` callback, any thread, any hook and any
/// TLS destructor pointing at addresses that are no longer mapped —
/// and Windows keeps delivering to them. Our own dock frames make one
/// instance of that certain rather than likely: they are *owned* by the
/// main window, so Win32 destroys them after the main window's
/// `WM_DESTROY` returns, which is after the host has dropped, and
/// destroying a frame destroys the plugin's client window with it. But
/// `NppExec` crashes without registering a dock at all, so enumerating
/// the cases is not a strategy.
///
/// What is lost by not unloading: nothing the OS does not do a moment
/// later. The process is exiting; every mapping goes with it.
/// `NPPN_SHUTDOWN` still fires while everything is still mapped — from
/// `WM_CLOSE` on Win32 and from the quit path on GTK and Cocoa — so a
/// plugin still gets its documented chance to save state.
///
/// DESIGN.md §6.4 records why the host does not unload a plugin on
/// exit.
///
/// Applied on every platform. The teardown crash was measured on
/// Windows, with a real third-party plugin, and the crash after a
/// refusal on Linux, with a test plugin; the POSIX analogue of the
/// teardown hazard is real too (an `atexit` handler, a pthread TLS
/// destructor or a signal handler registered by a `dlclose`d `.so`).
///
/// **The leak is per load, not per process.** Each backend builds
/// exactly one `Shell` and loads each plugin once, so a library is
/// mapped once and left mapped; a test suite that builds many hosts in
/// one process maps a library once per load. A future hot-reload or
/// restart-without-exit path would leak a mapping per cycle, silently;
/// such a path needs a real unload story (drain the plugin's windows
/// and timers first, then `FreeLibrary`) rather than this.
fn keep_mapped(lib: DynLib) -> ManuallyDrop<DynLib> {
    ManuallyDrop::new(lib)
}

/// A plugin picked for loading by [`PluginHost::next_pending_load`],
/// carrying everything [`execute_load`] needs so the host borrow can
/// be dropped before any plugin code runs. Its fields are private, so
/// outside this crate one can be had only from the host, and
/// [`PluginHost::commit_load`] takes it only if it is the load in
/// progress, field for field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingLoad {
    idx: usize,
    path: std::path::PathBuf,
    cmd_id_base: i32,
}

impl PendingLoad {
    /// Index into the host's plugin list.
    #[must_use]
    pub fn idx(&self) -> usize {
        self.idx
    }
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
        // `execute_load`, its library is still mapped (a library is
        // never unmapped once mapped: `keep_mapped`), and the
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
    ///
    /// Always runs to the end; [`Self::deliver_until`] can stop part-way.
    pub fn deliver(
        &self,
        npp_handle: crate::ffi::Hwnd,
        active_buffer: impl Fn() -> Option<usize>,
        restore_panels: impl FnOnce(),
    ) {
        self.deliver_until(npp_handle, active_buffer, restore_panels, || false);
    }

    /// [`Self::deliver`], ending part-way once `stop` answers `true`.
    ///
    /// `stop` is asked before every notification and before
    /// `restore_panels`; once it has answered `true`, nothing more is
    /// sent and the hook does not run. A backend passes "the host has
    /// been asked to quit": a plugin's handler may run a main loop of its
    /// own, the user may close the window inside it, and the quit's
    /// `NPPN_BEFORESHUTDOWN` and `NPPN_SHUTDOWN` must then be the last
    /// things any plugin hears — so the rest of the sequence is dropped
    /// rather than sent after them. Like the other two callbacks, `stop`
    /// must take no borrow the caller holds.
    pub fn deliver_until(
        &self,
        npp_handle: crate::ffi::Hwnd,
        active_buffer: impl Fn() -> Option<usize>,
        restore_panels: impl FnOnce(),
        stop: impl Fn() -> bool,
    ) {
        if self.plugins.is_empty() {
            return;
        }
        for ready in &self.plugins {
            if stop() {
                return;
            }
            ready.notify(npp_handle, NPPN_TBMODIFICATION, 0);
        }
        if stop() {
            return;
        }
        if catch_unwind(AssertUnwindSafe(restore_panels)).is_err() {
            tracing::warn!("restoring plugin dock panels panicked; continuing to NPPN_READY");
        }
        for ready in &self.plugins {
            if stop() {
                return;
            }
            if let Some(buffer) = active_buffer() {
                ready.notify(npp_handle, NPPN_BUFFERACTIVATED, buffer);
            }
        }
        for ready in &self.plugins {
            if stop() {
                return;
            }
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
/// `dispatch` is the backend's router, off Windows. The plugin is not
/// given it as it is but through a route of its own
/// ([`crate::plugin_route`]), so every message it ever sends is marked as
/// its own — for the first [`crate::MAX_ROUTED_PLUGINS`] plugins found;
/// past those, through the callback they share, which marks none.
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
    // And whatever it sends later, from wherever it runs — a signal
    // handler or timer of its own, which no call of the host's marks —
    // for the first `MAX_ROUTED_PLUGINS` plugins found.
    let dispatch = dispatch.map(|router| crate::caller::plugin_route(pending.idx, router));
    load_inner(pending, npp_data, dispatch)
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

/// Install the host's message-routing callback into a freshly-loaded
/// plugin, so its `SendMessage` transport reaches the host.
///
/// Only meaningful off Windows: there the plugin exports
/// `codepp_plugin_set_dispatch` (from `codepp-plugin-sdk`) and has no OS
/// message pump, and `dispatch` is the plugin's own route to the
/// backend's router, which [`execute_load`] chose. On Windows `dispatch`
/// is `None` and the symbol is absent (the SDK's export is
/// `#[cfg(not(windows))]`), so this is a no-op. A plugin that doesn't use
/// our SDK simply won't have the symbol and can't talk back to the host —
/// acceptable, since Linux only ever loads SDK-built cdylibs.
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
/// A refused plugin's library stays mapped, as every library does once
/// it is mapped ([`keep_mapped`]): it has run its `DllMain` and
/// `isUnicode`, and either may have started something that would
/// outlive an unmap. Notepad++ unmaps it.
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

/// Resolve the six entry points and run the initial setInfo +
/// getFuncsArray dance. Returns a fully-populated `LoadedPlugin` on
/// success. `cmd_id_base` is the first menu-command id assigned to
/// the plugin's `FuncItems` — incremented by one per item, written
/// back through the plugin's pointer so the plugin's own copy of
/// `_cmdID` matches the value the host installs in the menu.
fn load_inner(
    pending: &PendingLoad,
    npp_data: NppData,
    dispatch: Option<crate::ffi::HostDispatchFn>,
) -> Result<LoadedPlugin, String> {
    // Cap on the FuncItem count a plugin can contribute. Hoisted
    // above all statements (clippy's `items_after_statements`)
    // so the constant declaration sits with the function's
    // documentation rather than appearing mid-body. The cap is
    // a DoS guard — `i32::MAX` from a hostile or broken plugin
    // would otherwise trigger a ~17 GB `Vec::with_capacity`.
    const MAX_FUNCITEMS: i32 = 1024;

    let path = pending.path.as_path();
    let cmd_id_base = pending.cmd_id_base;
    // Mapped for good from here on, whatever becomes of the load: the
    // refusals below, before and after `setInfo`, included
    // (`keep_mapped`).
    let lib = keep_mapped(DynLib::load(path)?);

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
    // A plugin may contribute no menu items: it may still be useful
    // through beNotified-only lifecycles. One outcome is built below
    // for both cases, so every outcome carries its load.
    let (funcs, shortcut_defaults) = if raw.is_null() || count <= 0 {
        (Vec::new(), Vec::new())
    } else {
        // Checked before any id is written back into the plugin's items:
        // an id past the range would be one of Notepad++'s Scintilla key
        // commands or a plugin's allocated id.
        if !fits_cmd_id_range(cmd_id_base, count) {
            return Err(format!(
                "its {count} menu commands do not fit: plugin command ids end at \
                 {PLUGIN_CMD_ID_LIMIT}, and the next free one is {cmd_id_base}"
            ));
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
        unsafe {
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
        }
    };

    Ok(LoadedPlugin {
        load: pending.clone(),
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

/// Whether `count` menu commands numbered from `base` all fall in the
/// `FuncItem` range, [`PLUGIN_CMD_ID_BASE`] up to
/// [`PLUGIN_CMD_ID_LIMIT`]. `base` comes from the host's own counter,
/// which never leaves the range, so the lower bound only states it: a
/// base below the range would number a plugin's items among Code++'s
/// own commands.
fn fits_cmd_id_range(base: i32, count: i32) -> bool {
    base >= PLUGIN_CMD_ID_BASE
        && base
            .checked_add(count)
            .is_some_and(|end| end <= PLUGIN_CMD_ID_LIMIT)
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

// The shortcut snapshot and the id allocators, on every platform. The
// load is tested in `commit_tests` (the latch, a file that fails to
// map, an index past the registry) and, against real plugins, in
// `id_range_e2e` and `tests/`; discovery in `discovery_tests`.
#[cfg(test)]
mod tests {
    use super::*;

    /// The one unsafe dereference the shortcut path adds: NULL and
    /// zero-key pointers yield `None`, a real chord copies by value.
    /// (Reading one out of a real plugin's `FuncItem`s needs a bundled
    /// plugin that declares a default shortcut, which none does yet.)
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

// The command-id ranges: plain numbers, and the checks hold on every
// backend.
#[cfg(test)]
mod cmd_id_range_tests {
    use super::{
        fits_cmd_id_range, is_plugin_cmd_id, PLUGIN_ALLOC_CMD_BASE, PLUGIN_ALLOC_CMD_LIMIT,
        PLUGIN_CMD_ID_BASE, PLUGIN_CMD_ID_LIMIT,
    };

    /// The ids Notepad++ numbers above its plugin ranges, any of which
    /// a plugin can name in `NPPM_MENUCOMMAND`: its menu sections,
    /// from its `IDM` to the end of its Run menu's, and the two blocks
    /// its `resource.h` reserves above those, `IDCMD` to `IDCMD_LIMIT`
    /// and `IDSCINTILLA` to `IDSCINTILLA_LIMIT`.
    const NPP_COMMANDS: [std::ops::RangeInclusive<i32>; 3] =
        [40_000..=49_999, 50_000..=50_020, 60_000..=60_030];

    #[test]
    fn plugin_command_ids_are_none_of_notepad_plus_pluses() {
        for id in 0..=i32::from(u16::MAX) {
            if is_plugin_cmd_id(id) {
                let clash = NPP_COMMANDS.iter().find(|r| r.contains(&id));
                assert!(clash.is_none(), "{id} is a plugin id inside {clash:?}");
            }
        }
    }

    #[test]
    fn plugin_command_ids_are_the_funcitem_range_and_the_pool() {
        for (id, expected) in [
            (PLUGIN_CMD_ID_BASE - 1, false),
            (PLUGIN_CMD_ID_BASE, true),
            (PLUGIN_CMD_ID_LIMIT - 1, true),
            (PLUGIN_CMD_ID_LIMIT, false),
            (PLUGIN_ALLOC_CMD_BASE - 1, false),
            (PLUGIN_ALLOC_CMD_BASE, true),
            (PLUGIN_ALLOC_CMD_LIMIT - 1, true),
            (PLUGIN_ALLOC_CMD_LIMIT, false),
            (-1, false),
            (i32::MAX, false),
        ] {
            assert_eq!(is_plugin_cmd_id(id), expected, "{id}");
        }
        // Every plugin id travels in a `WM_COMMAND`'s 16 bits.
        assert!(PLUGIN_ALLOC_CMD_LIMIT <= i32::from(u16::MAX) + 1);
        // The ranges keep their order, so neither can reach the other.
        const { assert!(PLUGIN_CMD_ID_LIMIT <= PLUGIN_ALLOC_CMD_BASE) };
    }

    #[test]
    fn a_plugin_s_commands_must_fit_the_funcitem_range() {
        let room = PLUGIN_CMD_ID_LIMIT - PLUGIN_CMD_ID_BASE;
        for (base, count, expected) in [
            (PLUGIN_CMD_ID_BASE, 1, true),
            (PLUGIN_CMD_ID_BASE, room, true),
            (PLUGIN_CMD_ID_BASE, room + 1, false),
            (PLUGIN_CMD_ID_LIMIT - 1, 1, true),
            (PLUGIN_CMD_ID_LIMIT - 1, 2, false),
            (PLUGIN_CMD_ID_LIMIT, 1, false),
            // A base below the range would number the plugin's items
            // among Code++'s own commands.
            (PLUGIN_CMD_ID_BASE - 1, 1, false),
            (0, 1, false),
            (i32::MAX, 1, false),
        ] {
            assert_eq!(
                fits_cmd_id_range(base, count),
                expected,
                "{count} from {base}"
            );
        }
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

    std::thread_local! {
        /// `(plugin, code)` in the order the stop-test recorders saw
        /// them — per thread, so the test cannot see another's.
        static HEARD: std::cell::RefCell<Vec<(char, u32)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    fn heard(who: char, sci: *const SCNotification) {
        // SAFETY: `LoadNotifications` hands a live `SCNotification`.
        let code = unsafe { (*sci).nmhdr.code };
        HEARD.with(|h| h.borrow_mut().push((who, code)));
    }
    unsafe extern "C" fn heard_a(sci: *const SCNotification) {
        heard('a', sci);
    }
    unsafe extern "C" fn heard_b(sci: *const SCNotification) {
        heard('b', sci);
    }

    /// Deliver to two plugins, stopping once `stop_after` notifications
    /// have been heard (`'*'` marks the restore hook), and return what
    /// was heard.
    fn deliver_stopping_after(stop_after: usize) -> Vec<(char, u32)> {
        let mut notices = LoadNotifications::default();
        notices.push(ready(0, heard_a));
        notices.push(ready(1, heard_b));
        HEARD.with(|h| h.borrow_mut().clear());
        notices.deliver_until(
            core::ptr::null_mut(),
            || Some(7),
            || HEARD.with(|h| h.borrow_mut().push(('*', 0))),
            || HEARD.with(|h| h.borrow().len() >= stop_after),
        );
        HEARD.with(|h| h.borrow().clone())
    }

    /// Once `stop` answers `true`, nothing more is sent — not the rest of
    /// a broadcast, not the restore hook, not a later broadcast. A quit
    /// asked for from inside a handler relies on it: its shutdown pair
    /// must be the last thing any plugin hears.
    #[test]
    fn deliver_until_sends_nothing_once_stopped() {
        // Each stop is checked *before* the send it guards: a check moved
        // after the send would still deliver the first notification here,
        // and the first READY in the `5` case.
        assert_eq!(
            deliver_stopping_after(0),
            vec![],
            "a stop raised before delivery began still sent something"
        );
        assert_eq!(
            deliver_stopping_after(1),
            vec![('a', NPPN_TBMODIFICATION)],
            "a stop after the first notification still sent more"
        );
        assert_eq!(
            deliver_stopping_after(2),
            vec![('a', NPPN_TBMODIFICATION), ('b', NPPN_TBMODIFICATION)],
            "a stop before the restore hook still ran it"
        );
        assert_eq!(
            deliver_stopping_after(3),
            vec![
                ('a', NPPN_TBMODIFICATION),
                ('b', NPPN_TBMODIFICATION),
                ('*', 0),
            ],
            "a stop raised by the restore hook still sent NPPN_BUFFERACTIVATED"
        );
        assert_eq!(
            deliver_stopping_after(5),
            vec![
                ('a', NPPN_TBMODIFICATION),
                ('b', NPPN_TBMODIFICATION),
                ('*', 0),
                ('a', NPPN_BUFFERACTIVATED),
                ('b', NPPN_BUFFERACTIVATED),
            ],
            "a stop raised by the last NPPN_BUFFERACTIVATED still sent NPPN_READY"
        );
        assert_eq!(
            deliver_stopping_after(6),
            vec![
                ('a', NPPN_TBMODIFICATION),
                ('b', NPPN_TBMODIFICATION),
                ('*', 0),
                ('a', NPPN_BUFFERACTIVATED),
                ('b', NPPN_BUFFERACTIVATED),
                ('a', NPPN_READY),
            ],
            "a stop between two NPPN_READYs still sent the second"
        );
        assert_eq!(
            deliver_stopping_after(usize::MAX).len(),
            7,
            "a stop that never answers `true` must deliver everything"
        );
    }

    /// A plugin's `setInfo` and `getFuncsArray` run marked as the plugin
    /// being loaded, so a dock panel registered from `setInfo` is known
    /// to be its own, and the plugin is given its own route rather than
    /// the backend's router, so everything it sends later is too. A
    /// source check, because observing either needs a real DLL that
    /// reports it — `tests/example_hello_e2e.rs` does, for the route, on
    /// Linux.
    #[test]
    fn a_plugin_loads_as_itself() {
        // LF only, because the end of the body is found by `"\n}\n"`.
        // `.gitattributes` checks `*.rs` out as LF, but a working copy
        // checked out before the rule arrived keeps its CRLF files until
        // they change.
        let src = include_str!("host.rs").replace("\r\n", "\n");
        let body = &src[src.find("pub fn execute_load(").expect("execute_load")..];
        let body = &body[..body.find("\n}\n").expect("end of execute_load")];
        // Code only, so a line commented out — or a call named in a
        // comment — cannot stand in for the call itself.
        let body = body
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let mark = body
            .find("CallingPlugin::enter(pending.idx)")
            .expect("the load no longer runs marked as the plugin being loaded");
        let load = body.find("load_inner(").expect("the load itself");
        assert!(
            mark < load,
            "the mark must be set before the plugin's code runs"
        );
        // Whitespace dropped, so rustfmt's layout of the line does not
        // matter.
        let compact: String = body.split_whitespace().collect();
        let routed = compact
            .find(concat!(
                "letdispatch=dispatch.map(",
                "|router|crate::caller::plugin_route(pending.idx,router));",
            ))
            .expect("the plugin is no longer given a route of its own");
        let compact_load = compact.find("load_inner(").expect("the load itself");
        assert!(
            routed < compact_load,
            "the plugin must be given its route before its code runs"
        );
        assert!(
            compact[compact_load..].starts_with("load_inner(pending,npp_data,dispatch)"),
            "the load no longer installs the dispatch the route was chosen for"
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

    /// The body of `fn name` in this file, as code only and without
    /// whitespace, so neither a comment nor rustfmt's layout decides a
    /// guard's outcome. The body ends at the first `}` indented as far
    /// as the `fn` line, so a method ends at its own brace rather than
    /// its `impl` block's.
    fn compact_body(name: &str) -> String {
        let src = include_str!("host.rs").replace("\r\n", "\n");
        let sig = format!("fn {name}(");
        let at = src.find(&sig).unwrap_or_else(|| panic!("no fn {name}"));
        let line_start = src[..at].rfind('\n').map_or(0, |i| i + 1);
        let indent: String = src[line_start..at]
            .chars()
            .take_while(|&c| c == ' ')
            .collect();
        let body = &src[at..];
        let end = body
            .find(&format!("\n{indent}}}\n"))
            .unwrap_or_else(|| panic!("no end of fn {name}"));
        body[..end]
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<String>()
            .split_whitespace()
            .collect()
    }

    /// A plugin's items are numbered only once they are known to fit
    /// the `FuncItem` range: the check runs, and refuses the load,
    /// before the loop that writes each id back into the plugin's
    /// items. `id_range_e2e` below runs it against the real
    /// `example-hello` on Linux; this pins the order everywhere.
    #[test]
    fn a_plugin_s_ids_are_checked_before_they_are_written() {
        let compact = compact_body("load_inner");
        let check = compact
            .find("if!fits_cmd_id_range(cmd_id_base,count){returnErr(format!(")
            .expect("the load no longer refuses commands that do not fit");
        let write = compact
            .find("(*raw.add(i)).cmd_id=id;")
            .expect("the load no longer writes each id back");
        assert!(
            check < write,
            "the ids must be checked before any is written into the plugin's items"
        );
    }

    /// Once a library is mapped, nothing unmaps it. `load_inner` is the
    /// one place in this file that maps one, it gives the library up
    /// (`keep_mapped`) in the statement that maps it, nothing here takes
    /// it back or drops it in place, and a `LoadedPlugin` holds no handle
    /// to it. So neither a refusal before the init entry points finish
    /// (a missing entry point, a plugin refused as ANSI), nor one after
    /// them, nor an outcome `commit_load` refuses, nor a load a backend
    /// never commits can unmap it under whatever its `DllMain` or its
    /// init left running. `id_range_e2e` below sees a library refused
    /// before `setInfo`, one refused after it and an uncommitted one stay
    /// mapped on Linux; this pins the shape everywhere.
    #[test]
    fn a_library_is_never_unmapped_once_mapped() {
        // The production half of this file, which ends at its first test
        // module, read as code only. No other crate maps a plugin.
        let src = include_str!("host.rs").replace("\r\n", "\n");
        let production: String = src[..src.find("\n#[cfg(test)]\nmod ").expect("a test module")]
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<String>()
            .split_whitespace()
            .collect();
        assert_eq!(
            production.matches("DynLib::load(").count(),
            1,
            "a plugin's library is mapped somewhere other than load_inner"
        );
        assert_eq!(
            production.matches("ManuallyDrop::").count(),
            production.matches("ManuallyDrop::new(").count(),
            "something takes a library back out of keep_mapped's hands"
        );
        assert!(
            !production.contains("drop_in_place"),
            "something drops a library in place"
        );
        let load = compact_body("load_inner");
        assert_eq!(
            load.matches("DynLib::load(").count(),
            1,
            "load_inner maps a library a second way"
        );
        assert!(
            load.contains("letlib=keep_mapped(DynLib::load(path)?);"),
            "the library is not given up in the statement that maps it"
        );
        assert!(
            !load.contains("ManuallyDrop::"),
            "load_inner takes the library back out of keep_mapped's hands"
        );
        assert!(
            compact_body("keep_mapped").ends_with("->ManuallyDrop<DynLib>{ManuallyDrop::new(lib)"),
            "keep_mapped no longer keeps the library from being dropped"
        );
        let fields = &src[src.find("pub struct LoadedPlugin {").expect("LoadedPlugin")..];
        let fields: String = fields[..fields.find("\n}\n").expect("the struct's end")]
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect();
        assert!(
            !fields.contains("DynLib"),
            "a LoadedPlugin holds its library again, so dropping one unmaps it"
        );
    }
}

/// The load latch, on every platform: what [`PluginHost::commit_load`]
/// takes and refuses, what a commit releases and reserves, which
/// plugin a pass hands out, and what [`PluginHost::load_blocking`] does
/// with a plugin it must not load. The fixtures are not libraries:
/// discovery reads only their names, a load of one fails to map it, and
/// a load that succeeds is staged from entry points that do nothing
/// (`loaded_without_a_library`).
#[cfg(test)]
mod commit_tests {
    use super::*;

    /// A host that has discovered two plugins, `alpha` and `beta`, so a
    /// test can see both that the first is not offered again and that
    /// the next one is.
    fn host_with_two_plugins() -> (tempfile::TempDir, PluginHost) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        for name in ["alpha", "beta"] {
            let folder = dir.path().join(name);
            std::fs::create_dir_all(&folder).expect("the plugin's folder");
            std::fs::write(folder.join(format!("{name}.{PLUGIN_EXTENSION}")), b"x")
                .expect("the plugin's file");
        }
        let mut host = PluginHost::default();
        assert_eq!(host.discover(dir.path()).expect("discovery"), 2);
        (dir, host)
    }

    /// A `PendingLoad` changed since it was handed out, to another
    /// plugin's path or a base the host did not hand out, is not the
    /// load in progress. Committing it would file this library under
    /// another plugin's entry, or number the plugin's items over
    /// another plugin's. It is refused whatever its outcome, and it
    /// changes nothing: the plugin is neither loaded nor failed, the
    /// latch stays with the load still running, and that load's own
    /// commit goes through afterwards, having used no ids.
    #[test]
    fn a_load_changed_since_it_was_handed_out_is_refused() {
        let (_dir, mut host) = host_with_two_plugins();
        let alpha = host.next_pending_load().expect("alpha, pending");
        let beta_path = host.iter().nth(1).expect("beta").path.clone();
        let room = PLUGIN_CMD_ID_LIMIT - PLUGIN_CMD_ID_BASE;
        let mut forgeries = vec![(
            "another plugin's path".to_string(),
            PendingLoad {
                path: beta_path,
                ..alpha.clone()
            },
        )];
        for shift in [-1, 1, 3, room] {
            forgeries.push((
                format!("its base moved by {shift}"),
                PendingLoad {
                    cmd_id_base: alpha.cmd_id_base + shift,
                    ..alpha.clone()
                },
            ));
        }
        for (change, forged) in &forgeries {
            let err = host
                .commit_load(forged, Err("forged".to_string()))
                .expect_err("a load changed since it was handed out was committed");
            assert!(err.contains("not the load in progress"), "{change}: {err}");
            let first = host.iter().next().expect("alpha");
            assert!(
                !first.is_loaded() && first.failed_reason().is_none(),
                "{change}: the refused commit changed what is recorded for alpha"
            );
            assert!(
                host.next_pending_load().is_none(),
                "{change}: the refused commit released alpha's latch"
            );
        }
        let err = host
            .commit_load(&alpha, Err("alpha's own error".to_string()))
            .expect_err("a failed load");
        assert_eq!(err, "alpha's own error", "alpha's own commit was refused");
        let beta = host.next_pending_load().expect("beta, pending");
        assert_eq!(beta.idx, 1, "the next load is not beta's");
        assert_eq!(
            beta.cmd_id_base, PLUGIN_CMD_ID_BASE,
            "a refused commit used command ids"
        );
    }

    /// Only the load in progress is committed, and only once. A
    /// `PendingLoad` committed already, or one the host did not hand
    /// out, is refused, changes nothing recorded, and leaves the latch
    /// to the load still running: released, it would let a nested pass
    /// hand out a second load numbered from the same base.
    #[test]
    fn only_the_load_in_progress_is_committed() {
        let (_dir, mut host) = host_with_two_plugins();
        let alpha = host.next_pending_load().expect("alpha, pending");
        let not_handed_out = PendingLoad {
            idx: alpha.idx + 1,
            ..alpha.clone()
        };
        let err = host
            .commit_load(&not_handed_out, Err("forged".to_string()))
            .expect_err("a load the host did not hand out was committed");
        assert!(err.contains("not the load in progress"), "{err}");
        assert!(
            host.next_pending_load().is_none(),
            "a load the host did not hand out released alpha's latch"
        );
        let _ = host.commit_load(&alpha, Err("alpha's own error".to_string()));
        let beta = host.next_pending_load().expect("beta, pending");
        assert_ne!(beta.idx, alpha.idx, "the failed plugin was offered again");
        let err = host
            .commit_load(&alpha, Err("committed again".to_string()))
            .expect_err("a load committed already was committed again");
        assert!(err.contains("not the load in progress"), "{err}");
        assert!(
            host.iter()
                .next()
                .expect("alpha")
                .failed_reason()
                .is_some_and(|r| r == "alpha's own error"),
            "the second commit changed what was recorded for alpha"
        );
        assert!(
            host.next_pending_load().is_none(),
            "the second commit of alpha released beta's latch"
        );
        let err = host
            .commit_load(&beta, Err("beta's own error".to_string()))
            .expect_err("a failed load");
        assert_eq!(err, "beta's own error", "beta's own commit was refused");
    }

    /// No window and no route into a host: these fixtures never get as
    /// far as an entry point.
    fn no_npp_data() -> NppData {
        NppData {
            npp_handle: std::ptr::null_mut(),
            scintilla_main_handle: std::ptr::null_mut(),
            scintilla_second_handle: std::ptr::null_mut(),
        }
    }

    /// `load_blocking` takes the latch as `next_pending_load` does, so
    /// its own commit is the load in progress: a file that is not a
    /// library is recorded as the failure it is, and the latch is
    /// released for the next load.
    #[test]
    fn a_blocking_load_commits_its_own_outcome() {
        let (_dir, mut host) = host_with_two_plugins();
        let err = host
            .load_blocking(0, no_npp_data(), None)
            .expect_err("a file that is not a library loaded");
        assert!(!err.contains("not the load in progress"), "{err}");
        let alpha = host.iter().next().expect("alpha");
        assert_eq!(alpha.failed_reason(), Some(err.as_str()));
        let next = host
            .next_pending_load()
            .expect("the blocking load did not release the latch");
        assert_eq!(next.idx, 1, "the next load is not beta's");
    }

    /// A plugin that failed is not loaded again, since a second try
    /// could run its init twice. Its recorded reason comes back even
    /// with its file gone, so nothing was opened again.
    #[test]
    fn a_blocking_load_does_not_retry_a_failed_plugin() {
        let (dir, mut host) = host_with_two_plugins();
        let first = host
            .load_blocking(0, no_npp_data(), None)
            .expect_err("a file that is not a library loaded");
        let file = dir
            .path()
            .join("alpha")
            .join(format!("alpha.{PLUGIN_EXTENSION}"));
        std::fs::remove_file(file).expect("alpha's file");
        let second = host
            .load_blocking(0, no_npp_data(), None)
            .expect_err("a failed plugin loaded");
        assert_eq!(second, first, "the failed plugin was loaded again");
    }

    /// While another load is in progress, a blocking load is refused,
    /// and leaves that load's latch and its own plugin as they were.
    #[test]
    fn a_blocking_load_is_refused_while_another_is_in_progress() {
        let (_dir, mut host) = host_with_two_plugins();
        let alpha = host.next_pending_load().expect("alpha, pending");
        let err = host
            .load_blocking(1, no_npp_data(), None)
            .expect_err("a blocking load ran while another was in progress");
        assert!(err.contains("in progress"), "{err}");
        let beta = host.iter().nth(1).expect("beta");
        assert!(
            beta.failed_reason().is_none(),
            "the refused plugin was recorded as failed"
        );
        assert!(
            host.next_pending_load().is_none(),
            "the refusal released alpha's latch"
        );
        let err = host
            .commit_load(&alpha, Err("alpha's own error".to_string()))
            .expect_err("a failed load");
        assert_eq!(err, "alpha's own error", "alpha's commit was refused");
    }

    // Entry points for a plugin the tests record as loaded without
    // mapping a library. None of them runs on the paths these tests
    // take.
    extern "C" fn no_info(_: NppData) {}
    extern "C" fn no_name() -> *const u16 {
        std::ptr::null()
    }
    extern "C" fn no_items(_: *mut i32) -> *mut FuncItem {
        std::ptr::null_mut()
    }
    extern "C" fn no_notification(_: *const SCNotification) {}
    extern "C" fn no_message(_: u32, _: usize, _: isize) -> isize {
        0
    }
    extern "C" fn unicode() -> i32 {
        1
    }

    /// The outcome of `pending` succeeding, with `items` menu items and
    /// the plugin's file stem for its name, from entry points that do
    /// nothing, so a test can commit a success without a library.
    fn loaded_without_a_library(pending: &PendingLoad, items: usize) -> LoadedPlugin {
        let item = FuncItem {
            item_name: [0; crate::ffi::MENU_TITLE_LENGTH],
            p_func: None,
            cmd_id: 0,
            init2_check: 0,
            p_sh_key: std::ptr::null_mut(),
        };
        LoadedPlugin {
            load: pending.clone(),
            set_info: no_info,
            get_name: no_name,
            get_funcs_array: no_items,
            be_notified: no_notification,
            message_proc: no_message,
            is_unicode: unicode,
            funcs: vec![item; items],
            shortcut_defaults: vec![None; items],
            name: pending
                .path
                .file_stem()
                .expect("a plugin file")
                .to_string_lossy()
                .into_owned(),
        }
    }

    /// A load that succeeded releases the latch as one that failed
    /// does, so the next plugin is offered once a load has gone
    /// through.
    #[test]
    fn a_load_that_succeeded_releases_the_latch() {
        let (_dir, mut host) = host_with_two_plugins();
        let alpha = host.next_pending_load().expect("alpha, pending");
        let ready = host
            .commit_load(&alpha, Ok(loaded_without_a_library(&alpha, 0)))
            .expect("a load that succeeded was refused");
        assert!(
            ready.is_some(),
            "a load that succeeded has nothing to deliver"
        );
        let first = host.iter().next().expect("alpha");
        assert!(first.is_loaded(), "the load that succeeded is not recorded");
        assert_eq!(
            first.name.as_deref(),
            Some("alpha"),
            "the loaded plugin's name is not recorded"
        );
        let beta = host
            .next_pending_load()
            .expect("a load that succeeded kept the latch");
        assert_eq!(beta.idx, 1, "the next load is not beta's");
    }

    /// `has_load_to_start` holds while a plugin is neither loaded, failed
    /// nor disabled and no load is in progress, which is when
    /// `next_pending_load` hands one out. It takes no latch: asking it
    /// changes what the loader hands out next in no way.
    #[test]
    fn a_load_can_start_while_a_plugin_waits_and_none_is_in_progress() {
        let (_dir, mut host) = host_with_two_plugins();
        assert!(host.has_load_to_start(), "two plugins, none loaded");
        let alpha = host.next_pending_load().expect("alpha, pending");
        assert!(
            !host.has_load_to_start(),
            "alpha's load is in progress, so a pass started now loads nothing"
        );
        host.commit_load(&alpha, Ok(loaded_without_a_library(&alpha, 0)))
            .expect("alpha's own outcome was refused");
        assert!(host.has_load_to_start(), "beta still waits");
        assert!(host.has_load_to_start(), "asking twice changed the answer");
        let beta = host
            .next_pending_load()
            .expect("asking took the latch or handed beta out");
        assert_eq!(beta.idx, 1, "the next load is not beta's");
        assert!(!host.has_load_to_start(), "beta's load is in progress");
        let _ = host.commit_load(&beta, Err("beta's own error".to_string()));
        assert!(
            !host.has_load_to_start(),
            "one plugin loaded and one failed, and a load can still start"
        );

        let (_dir, mut host) = host_with_two_plugins();
        assert!(host.set_disabled(0, true) && host.set_disabled(1, true));
        assert!(
            !host.has_load_to_start(),
            "both plugins disabled, and a load can still start"
        );
        assert!(host.set_disabled(1, false));
        assert!(host.has_load_to_start(), "beta, re-enabled, does not wait");

        // A load that never commits leaves its latch set, and no later
        // pass gets past it: nothing can start, for good.
        let (_dir, mut host) = host_with_two_plugins();
        let _never_committed = host.next_pending_load().expect("alpha, pending");
        assert!(
            !host.has_load_to_start(),
            "a load started past a latch left set"
        );
        assert!(host.next_pending_load().is_none());
        assert!(!host.has_load_to_start(), "asking changed the answer");
    }

    /// A commit reserves the ids of the plugin's items, so the next
    /// load is numbered after them.
    #[test]
    fn a_commit_reserves_the_ids_of_its_items() {
        let (_dir, mut host) = host_with_two_plugins();
        let alpha = host.next_pending_load().expect("alpha, pending");
        host.commit_load(&alpha, Ok(loaded_without_a_library(&alpha, 3)))
            .expect("a load that succeeded was refused");
        let beta = host.next_pending_load().expect("beta, pending");
        assert_eq!(
            beta.cmd_id_base,
            PLUGIN_CMD_ID_BASE + 3,
            "the commit did not reserve the ids of alpha's items"
        );
    }

    /// The outcome committed must be the load's own. Another load's,
    /// here an earlier load's committed for beta's, or one numbered
    /// from another base, would file that library under beta's entry,
    /// or install items numbered from ids beta was not given. It is
    /// refused, changes nothing recorded, and leaves the latch with
    /// beta, whose own outcome then goes through.
    #[test]
    fn an_outcome_of_another_load_is_refused() {
        let (_dir, mut host) = host_with_two_plugins();
        let alpha = host.next_pending_load().expect("alpha, pending");
        let _ = host.commit_load(&alpha, Err("alpha's own error".to_string()));
        let beta = host.next_pending_load().expect("beta, pending");
        let others = [
            ("an earlier load's", alpha.clone()),
            (
                "one numbered from another base",
                PendingLoad {
                    cmd_id_base: beta.cmd_id_base + 1,
                    ..beta.clone()
                },
            ),
        ];
        for (whose, other) in &others {
            let err = host
                .commit_load(&beta, Ok(loaded_without_a_library(other, 1)))
                .expect_err("another load's outcome was committed");
            assert!(err.contains("another load's"), "{whose}: {err}");
            let second = host.iter().nth(1).expect("beta");
            assert!(
                !second.is_loaded() && second.failed_reason().is_none(),
                "{whose}: the refused outcome changed what is recorded for beta"
            );
            assert!(
                host.next_pending_load().is_none(),
                "{whose}: the refused outcome released beta's latch"
            );
        }
        host.commit_load(&beta, Ok(loaded_without_a_library(&beta, 0)))
            .expect("beta's own outcome was refused");
        assert!(
            host.iter().nth(1).expect("beta").is_loaded(),
            "beta's own outcome is not recorded"
        );
    }

    /// A pass restricted to named plugins hands out only those: none
    /// for an empty set, and the one named, past one it does not name.
    #[test]
    fn a_restricted_pass_offers_only_the_plugins_it_names() {
        let (_dir, mut host) = host_with_two_plugins();
        assert!(
            host.next_pending_load_in(Some(&[])).is_none(),
            "a pass naming no plugin handed one out"
        );
        let beta_key = host.iter().nth(1).expect("beta").module_key();
        let pending = host
            .next_pending_load_in(Some(&[beta_key]))
            .expect("beta, pending");
        assert_eq!(pending.idx, 1, "a plugin the pass did not name was offered");
    }

    /// An index that names no plugin is an error, and takes no latch.
    #[test]
    fn a_blocking_load_of_no_plugin_is_an_error() {
        let (_dir, mut host) = host_with_two_plugins();
        let err = host
            .load_blocking(99, no_npp_data(), None)
            .expect_err("a blocking load of no plugin succeeded");
        assert!(err.contains("out of range"), "{err}");
        assert!(
            host.next_pending_load().is_some(),
            "the refusal took the latch"
        );
    }

    /// A blocking load leaves a plugin that is loaded already alone:
    /// loading it again would run its init a second time.
    #[test]
    fn a_blocking_load_leaves_a_loaded_plugin_alone() {
        let (_dir, mut host) = host_with_two_plugins();
        let alpha = host.next_pending_load().expect("alpha, pending");
        host.commit_load(&alpha, Ok(loaded_without_a_library(&alpha, 0)))
            .expect("a load that succeeded was refused");
        host.load_blocking(0, no_npp_data(), None)
            .expect("a loaded plugin was loaded again");
        assert!(
            host.iter().next().expect("alpha").is_loaded(),
            "the loaded plugin is no longer recorded as loaded"
        );
    }

    /// A blocking load leaves a disabled plugin alone, and takes no
    /// latch for it.
    #[test]
    fn a_blocking_load_leaves_a_disabled_plugin_alone() {
        let (_dir, mut host) = host_with_two_plugins();
        assert!(host.set_disabled(0, true), "alpha was disabled already");
        host.load_blocking(0, no_npp_data(), None)
            .expect("a disabled plugin was loaded");
        let alpha = host.iter().next().expect("alpha");
        assert!(
            !alpha.is_loaded() && alpha.failed_reason().is_none(),
            "a disabled plugin was loaded"
        );
        let beta = host
            .next_pending_load()
            .expect("the blocking load kept a latch for a disabled plugin");
        assert_eq!(beta.idx, 1, "the next load is not beta's");
    }
}

/// The command-id range, run against the real `example-hello`: a load
/// that does not fit is refused before any id is written and is not
/// offered again, an exact fit loads, a refused plugin and one never
/// committed keep their libraries mapped, a plugin refused before
/// `setInfo` keeps its library mapped while its own thread runs on, and
/// a load committed twice is refused the second time, even when its
/// items took no ids.
///
/// Inside the crate rather than beside `tests/example_hello_e2e.rs`,
/// because running out of room is staged by setting the host's own
/// counter, which only the crate can reach. A real host gets there
/// only once its plugins have numbered 9000 commands.
///
/// Linux only, like `tests/example_hello_e2e.rs`, and `#[ignore]`d for
/// the same reason: it needs `libexample_hello.so`, which a build of
/// `codepp-example-hello` makes and `cargo test -p codepp-plugin-host`
/// does not. The refusal before `setInfo` also compiles a fixture with
/// the C compiler, `cc`. Run it with
/// `cargo build -p codepp-example-hello && cargo test -p
/// codepp-plugin-host --lib id_range_e2e -- --ignored`. A test fails,
/// rather than skips, when the library or `cc` is not there.
#[cfg(all(test, target_os = "linux"))]
mod id_range_e2e {
    use super::*;

    /// The plugin, where a build with this test binary's target
    /// directory and profile puts it: beside the `deps` folder the test
    /// binary is in.
    fn built_plugin() -> PathBuf {
        let exe = std::env::current_exe().expect("the test binary's path");
        let profile = exe
            .parent()
            .and_then(Path::parent)
            .expect("the test binary is in <target>/<profile>/deps");
        let so = profile.join("libexample_hello.so");
        assert!(
            so.exists(),
            "{} is not built: run `cargo build -p codepp-example-hello` first, in \
             the tests' profile (`--release` for a release run)",
            so.display()
        );
        so
    }

    /// A copy of the plugin, in the layout discovery takes, and a host
    /// that has discovered it. A copy per test, so no two tests share a
    /// mapping.
    fn staged() -> (tempfile::TempDir, PathBuf, PluginHost) {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let folder = tmp.path().join("example_hello");
        std::fs::create_dir_all(&folder).expect("the plugin's folder");
        let path = folder.join("example_hello.so");
        std::fs::copy(built_plugin(), &path).expect("a copy of the plugin");
        let mut host = PluginHost::new();
        assert_eq!(host.discover(tmp.path()).expect("discovery"), 1);
        (tmp, path, host)
    }

    /// No window and no route into a host: the plugin's init needs
    /// neither, and a load with no route sends nothing.
    fn npp_data() -> NppData {
        NppData {
            npp_handle: std::ptr::null_mut(),
            scintilla_main_handle: std::ptr::null_mut(),
            scintilla_second_handle: std::ptr::null_mut(),
        }
    }

    /// The `_cmdID` of each of the plugin's items, read from the
    /// plugin's own array through a handle of the test's own.
    fn item_ids(lib: &DynLib) -> Vec<i32> {
        // SAFETY: `getFuncsArray` has the ABI `ffi` declares, and
        // returns the plugin's own static array, which `lib` keeps
        // mapped while it is read.
        unsafe {
            let get: GetFuncsArrayFn = lib.resolve("getFuncsArray").expect("getFuncsArray");
            let mut count = 0;
            let raw = get(&raw mut count);
            (0..usize::try_from(count).expect("a count"))
                .map(|i| (*raw.add(i)).cmd_id)
                .collect()
        }
    }

    /// How many items the plugin has.
    fn item_count(lib: &DynLib) -> i32 {
        i32::try_from(item_ids(lib).len()).expect("a count")
    }

    /// The control for the mapping checks: here a library whose plugin
    /// has run its init entry points, as a load runs them, unmaps when
    /// its `DynLib` drops. Without it, a library that stayed mapped for
    /// a reason of its own, its init's included, would pass a check
    /// that the host kept it mapped, whatever the host did with it.
    fn assert_a_dropped_library_is_unmapped() {
        let (_tmp, control, _host) = staged();
        let lib = DynLib::load(&control).expect("the plugin");
        assert!(is_mapped(&control), "a loaded library is not mapped");
        // SAFETY: each entry point has the ABI `ffi` declares. The
        // plugin's init needs no window, and with no route it sends
        // nothing.
        unsafe {
            let is_unicode: IsUnicodeFn = lib.resolve("isUnicode").expect("isUnicode");
            let get_name: GetNameFn = lib.resolve("getName").expect("getName");
            let set_info: SetInfoFn = lib.resolve("setInfo").expect("setInfo");
            assert_ne!(is_unicode(), 0, "example-hello is a Unicode plugin");
            let _ = get_name();
            set_info(npp_data());
        }
        assert!(item_count(&lib) > 0, "example-hello publishes commands");
        drop(lib);
        assert!(
            !is_mapped(&control),
            "a dropped library stays mapped here, so this test cannot see one unmapped"
        );
    }

    /// Whether this process has the file at `path` mapped.
    fn is_mapped(path: &Path) -> bool {
        let path = path.canonicalize().expect("the staged plugin");
        let path = path.to_str().expect("a UTF-8 path");
        std::fs::read_to_string("/proc/self/maps")
            .expect("/proc/self/maps")
            .lines()
            .any(|line| line.ends_with(path))
    }

    #[test]
    #[ignore = "needs libexample_hello.so: run `cargo build -p codepp-example-hello` first"]
    fn a_load_that_does_not_fit_is_refused_before_any_id_is_written() {
        let (_tmp, path, mut host) = staged();
        // A handle of the test's own, so the plugin's array can still
        // be read once the host's load is refused.
        let mine = DynLib::load(&path).expect("the plugin");
        let items = item_count(&mine);
        assert!(items > 1, "example-hello publishes more than one command");
        // One id short of room.
        let base = PLUGIN_CMD_ID_LIMIT - items + 1;
        host.next_cmd_id = base;
        let pending = host.next_pending_load().expect("the plugin, pending");
        assert_eq!(pending.cmd_id_base, base);
        let Err(err) = execute_load(&pending, npp_data(), None) else {
            panic!("a load with no room was not refused");
        };
        assert!(err.contains("do not fit"), "{err}");
        assert!(
            item_ids(&mine).iter().all(|&id| id == 0),
            "an id was written into the plugin's items before the refusal"
        );
        assert!(host.commit_load(&pending, Err(err)).is_err());
        let plugin = host.iter().next().expect("the plugin");
        assert!(plugin
            .failed_reason()
            .is_some_and(|r| r.contains("do not fit")));
        assert!(
            host.next_pending_load().is_none(),
            "a refused plugin was offered again"
        );
        assert_eq!(host.next_cmd_id, base, "a refused load used command ids");
    }

    #[test]
    #[ignore = "needs libexample_hello.so: run `cargo build -p codepp-example-hello` first"]
    fn a_load_that_fits_exactly_takes_the_last_ids_of_the_range() {
        let (_tmp, path, mut host) = staged();
        let mine = DynLib::load(&path).expect("the plugin");
        let items = item_count(&mine);
        host.next_cmd_id = PLUGIN_CMD_ID_LIMIT - items;
        let pending = host.next_pending_load().expect("the plugin, pending");
        let loaded = execute_load(&pending, npp_data(), None).expect("an exact fit loads");
        assert!(host.commit_load(&pending, Ok(loaded)).is_ok());
        let ids = item_ids(&mine);
        assert_eq!(ids.first().copied(), Some(PLUGIN_CMD_ID_LIMIT - items));
        assert_eq!(ids.last().copied(), Some(PLUGIN_CMD_ID_LIMIT - 1));
        assert_eq!(host.next_cmd_id, PLUGIN_CMD_ID_LIMIT);
    }

    #[test]
    #[ignore = "needs libexample_hello.so: run `cargo build -p codepp-example-hello` first"]
    fn a_refused_plugin_keeps_its_library_mapped() {
        assert_a_dropped_library_is_unmapped();
        let (_tmp, path, mut host) = staged();
        // No room at all.
        host.next_cmd_id = PLUGIN_CMD_ID_LIMIT;
        let pending = host.next_pending_load().expect("the plugin, pending");
        let Err(err) = execute_load(&pending, npp_data(), None) else {
            panic!("a load with no room was not refused");
        };
        assert!(err.contains("do not fit"), "{err}");
        assert!(
            is_mapped(&path),
            "a refused plugin's library was unmapped after its init ran"
        );
    }

    /// A backend that loses a load's outcome on the way to
    /// `commit_load` drops a `LoadedPlugin`, which unmaps nothing.
    #[test]
    #[ignore = "needs libexample_hello.so: run `cargo build -p codepp-example-hello` first"]
    fn a_load_never_committed_keeps_its_library_mapped() {
        assert_a_dropped_library_is_unmapped();
        let (_tmp, path, mut host) = staged();
        let pending = host.next_pending_load().expect("the plugin, pending");
        let loaded = execute_load(&pending, npp_data(), None).expect("the plugin loads");
        drop(loaded);
        assert!(
            is_mapped(&path),
            "a load that was never committed unmapped its plugin's library"
        );
    }

    #[test]
    #[ignore = "needs libexample_hello.so: run `cargo build -p codepp-example-hello` first"]
    fn a_load_committed_twice_is_refused_the_second_time() {
        let (_tmp, _path, mut host) = staged();
        let pending = host.next_pending_load().expect("the plugin, pending");
        let first = execute_load(&pending, npp_data(), None).expect("the plugin loads");
        assert!(host.commit_load(&pending, Ok(first)).is_ok());
        // A caller holding two outcomes for one load: the second is
        // numbered from the base the first commit used.
        let second = execute_load(&pending, npp_data(), None).expect("the plugin loads again");
        let err = host
            .commit_load(&pending, Ok(second))
            .expect_err("a load committed already was committed again");
        assert!(err.contains("not the load in progress"), "{err}");
        assert!(
            host.iter().next().expect("the plugin").is_loaded(),
            "the refused commit replaced the loaded plugin"
        );
    }

    /// A load committed already is refused even while the next free id
    /// is still the one it was numbered from, as it is after a plugin
    /// whose items took no ids: its first commit released the latch, so
    /// it is no longer the load in progress. A second commit would
    /// record its outcome over the loaded plugin, a failure here, which
    /// would stop its commands running. Staged by putting the counter
    /// back where the load found it, which is where such a plugin
    /// leaves it.
    #[test]
    #[ignore = "needs libexample_hello.so: run `cargo build -p codepp-example-hello` first"]
    fn a_plugin_whose_items_took_no_ids_is_committed_once() {
        let (_tmp, path, mut host) = staged();
        let pending = host.next_pending_load().expect("the plugin, pending");
        let loaded = execute_load(&pending, npp_data(), None).expect("the plugin loads");
        assert!(host.commit_load(&pending, Ok(loaded)).is_ok());
        host.next_cmd_id = pending.cmd_id_base;
        let err = host
            .commit_load(&pending, Err("committed again".to_string()))
            .expect_err("a load committed already was committed again");
        assert!(err.contains("not the load in progress"), "{err}");
        assert!(
            host.iter().next().expect("the plugin").is_loaded(),
            "the second commit replaced the loaded plugin"
        );
        assert!(
            is_mapped(&path),
            "the second commit unmapped the loaded plugin's library"
        );
    }

    /// A test plugin built by the C compiler at test time, for the two
    /// shapes no in-tree plugin has. By default its `isUnicode` starts a
    /// thread and returns FALSE, so it is refused as ANSI before
    /// `setInfo` with code of its own still running. Compiled with
    /// `-DITEMLESS` it is a Unicode plugin that publishes no menu items.
    const FIXTURE: &str = r"
#include <pthread.h>
#include <unistd.h>
static volatile long ticks;
static void *tick(void *arg) { (void)arg; for (;;) { ticks++; usleep(2000); } return 0; }
int isUnicode(void) {
#ifdef ITEMLESS
    return 1;
#else
    pthread_t t;
    if (pthread_create(&t, 0, tick, 0) == 0) pthread_detach(t);
    return 0;
#endif
}
struct NppData { void *npp, *main, *second; };
void setInfo(struct NppData data) { (void)data; }
const void *getName(void) { return 0; }
void *getFuncsArray(int *count) { *count = 0; return 0; }
void beNotified(void *notification) { (void)notification; }
long messageProc(unsigned msg, unsigned long wparam, long lparam) {
    (void)msg; (void)wparam; (void)lparam; return 1;
}
";

    /// The fixture, compiled with `cc` and the given flags and staged
    /// in the layout discovery takes as the plugin `name`, and a host
    /// that has discovered it.
    fn staged_fixture(name: &str, flags: &[&str]) -> (tempfile::TempDir, PathBuf, PluginHost) {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let src = tmp.path().join("fixture.c");
        std::fs::write(&src, FIXTURE).expect("the fixture's source");
        let plugins = tmp.path().join("plugins");
        let folder = plugins.join(name);
        std::fs::create_dir_all(&folder).expect("the fixture's folder");
        let path = folder.join(format!("{name}.so"));
        let status = std::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-pthread"])
            .args(flags)
            .arg("-o")
            .arg(&path)
            .arg(&src)
            .status()
            .expect("a C compiler, `cc`, on PATH");
        assert!(status.success(), "the fixture did not compile");
        let mut host = PluginHost::new();
        assert_eq!(host.discover(&plugins).expect("discovery"), 1);
        (tmp, path, host)
    }

    /// A plugin that publishes no menu items still loads: it may be
    /// useful through `beNotified` alone. Its outcome carries its load,
    /// as every outcome does, and it takes no command ids.
    #[test]
    #[ignore = "needs a C compiler, `cc`, on PATH"]
    fn a_plugin_with_no_items_loads_and_takes_no_ids() {
        let (_tmp, _path, mut host) = staged_fixture("itemless_fixture", &["-DITEMLESS"]);
        let base = host.next_cmd_id;
        let pending = host.next_pending_load().expect("the fixture, pending");
        let loaded = execute_load(&pending, npp_data(), None).expect("an item-less plugin loads");
        assert!(loaded.funcs.is_empty(), "the fixture publishes no items");
        assert_eq!(loaded.load, pending, "the outcome does not carry its load");
        assert!(host.commit_load(&pending, Ok(loaded)).is_ok());
        assert!(host.iter().next().expect("the fixture").is_loaded());
        assert_eq!(
            host.next_cmd_id, base,
            "an item-less plugin took command ids"
        );
    }

    /// The refusal round five found: before `setInfo`, a plugin's own
    /// code may already be running, from `DllMain` or `isUnicode`, so
    /// unmapping its library there crashes the host when that code next
    /// runs. The fixture's thread ticks every 2 ms, so a library
    /// unmapped by the refusal ends this test in a `SIGSEGV`.
    #[test]
    #[ignore = "needs libexample_hello.so and `cc`: run `cargo build -p codepp-example-hello` first"]
    fn a_plugin_refused_before_set_info_keeps_its_library_mapped() {
        assert_a_dropped_library_is_unmapped();
        let (_tmp, path, mut host) = staged_fixture("ansi_fixture", &[]);
        let pending = host.next_pending_load().expect("the fixture, pending");
        let Err(err) = execute_load(&pending, npp_data(), None) else {
            panic!("an ANSI plugin was not refused");
        };
        assert!(err.contains("isUnicode returned FALSE"), "{err}");
        assert!(host.commit_load(&pending, Err(err)).is_err());
        drop(host);
        // Long enough for the fixture's thread to tick many times.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            is_mapped(&path),
            "a plugin refused before setInfo had its library unmapped"
        );
    }
}
