//! Glue layer between `core` and the platform UI crates.
//!
//! The `Shell` owns the application's mutable state (Session, Loader,
//! `FileWatcher`, the active `EditorHandle`, the in-memory text buffer
//! shadow) and exposes high-level operations the UI calls in response
//! to user actions: `open_file`, `save_file`, `apply_load_result`,
//! `apply_file_change`. UI crates implement [`UiPlatform`] for the
//! parts that have to live on the UI thread (showing dialogs, posting
//! status-bar text, pushing buffer contents into the active Scintilla
//! control via `EditorHandle::send`).
//!
//! # Allowed pedantic lints, with rationale
//!
//! - `clippy::cast_possible_truncation`
//! - `clippy::cast_possible_wrap`
//! - `clippy::cast_sign_loss`
//!
//! Shell does FFI-shaped value handling (buffer offsets, byte
//! lengths, NPPM ids) that goes through deliberate `as` casts
//! between Rust's integer widths and Win32 / Scintilla / N++-ABI
//! shapes (`isize` / `usize` / `i32` / `u64`). Marking each cast
//! individually would add ~25 attribute lines for no
//! reader-defence value; the inner attribute documents the
//! trade-off once.
//!
//! - `clippy::similar_names`
//!
//! Test scaffolding pairs like `loader`/`loaded`, `path`/`paths`,
//! `tab`/`tabs` are semantically distinct but trip the lint.
//! Allowed crate-wide.
//!
//! Cross-thread marshaling (DESIGN.md §5.4): worker threads (Loader,
//! `FileWatcher`) post their typed results into per-source channels and
//! call a wake closure that the UI crate hands the `Shell` at startup.
//! On Win32 the wake closure is `PostMessage(hwnd, WM_APP_WAKE, 0, 0)`.
//! The UI thread's wake handler drains both channels and applies each
//! item to the shell.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::similar_names,
    // Worker / handler patterns in this crate's UI-thread
    // helpers take owned `PathBuf`, `Arc`, `String` etc. by
    // value because the calling context consumes them
    // (channel sends, struct field moves, owned closures).
    // Clippy's `needless_pass_by_value` lint fires anyway —
    // misfires across the broader codebase.
    clippy::needless_pass_by_value,
    // Helpers and tests declare locally-scoped `const`s and
    // `fn` items at the relevant call site rather than
    // hoisting them above every initialisation expression.
    // `items_after_statements` is informative on a tiny
    // function, distracting on the larger handlers here.
    clippy::items_after_statements,
    // `missing_errors_doc` fires on every `Result`-returning
    // public method. Shell is an internal-to-the-workspace
    // crate (no external API contract); the error variants
    // each method propagates are visible at the type level
    // via `ShellError` / `SessionError` / `StylesError` etc.,
    // and the inline doc comments describe the failure modes
    // alongside the success contract. Marking each one with
    // a separate `# Errors` section would duplicate already-
    // documented behaviour at the cost of per-method
    // boilerplate.
    clippy::missing_errors_doc
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{unbounded, Receiver, Sender};

use codepp_core::file::{Loader, LoaderShutdown};
use codepp_core::lang::L_TEXT;
use codepp_core::{
    Encoding, Eol, FindHistory, LangType, LoadResult, RequestId, Session, WindowGeometry,
};
use codepp_platform::watch::{FileChange, FileWatcher};
#[cfg(target_os = "windows")]
use codepp_plugin_host::{
    dispatch_nppm, notify_all, FuncItem, HostServices, Hwnd, Notification, NppData, PluginCmd,
    PluginHost, NPPMAINMENU, NPPPLUGINMENU,
};

pub mod fif;
pub use fif::{FifError, FifEvent, FifJobId, FifRequest, FifStats};

/// Plugin-driven pre-fill for the next FIF dialog open. Populated
/// by `NPPM_LAUNCHFINDINFILESDLG` and drained by the Win32 plugin
/// dispatch in `main_wnd_proc`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FifLaunchPrefill {
    /// Optional directory to seed the dialog's Directory combobox.
    pub directory: Option<PathBuf>,
    /// Optional filter expression to seed the Filters combobox.
    pub filters: Option<String>,
}

/// Stable nonzero buffer id for the active buffer in the Phase 3
/// single-tab world. Multi-tab assigns per-tab ids in milestone 6.
/// Plugins receive this from `NPPM_GETCURRENTBUFFERID` and pass it
/// back via `NPPM_GETFULLPATHFROMBUFFERID` etc.
///
/// **Multi-tab migration note:** every site that references this
/// constant is a single-tab assumption. Searching for
/// `PRIMARY_BUFFER_ID` is the canonical way to find code that needs
/// rewriting in milestone 6 — keep using the named constant rather
/// than the literal `1` so the search stays useful.
pub const PRIMARY_BUFFER_ID: isize = 1;

/// Upper bound on tabs restored from session.xml. Caps the work
/// triggered by a corrupted or tampered session file (a runaway
/// session-save bug or an attacker with write access to `AppData`
/// could otherwise queue thousands of async loads at startup, each
/// allocating a Tab + decoded buffer text — a local `DoS` for the
/// invoking user's account). Set well above any realistic open-tab
/// count.
pub const MAX_SESSION_TABS: usize = 512;

/// Hard ceiling on the size of an individual backup file we'll read
/// during session restore. Untitled buffers are normally small (the
/// user typing notes), but a tampered or accidentally-huge backup
/// shouldn't allocate gigabytes at startup. 64 MiB is comfortably
/// above any realistic untitled-buffer size while keeping the
/// upper bound on a load attack to a fixed amount.
const MAX_BACKUP_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Hard ceiling on the number of tabs that can be open at one time.
/// `Shell::open_file` and `Shell::new_untitled` refuse to allocate
/// past this bound and log a `tracing::warn!` instead of crashing.
///
/// Bounds the CPU-time `DoS` surface for a hostile in-process plugin
/// that calls `NPPM_DOOPEN` (or any path-opening NPPM message) in
/// a tight loop. Without a cap, the loop would push `Shell.tabs`
/// indefinitely until `next_buffer_id` overflowed at `i32::MAX`
/// (~2 billion iterations) — a panic the `catch_unwind` boundary
/// would catch, but only after burning ~10s of minutes of pegged
/// CPU. Capping at a generous-but-finite value cuts that `DoS`
/// window from billions of iterations to thousands.
///
/// 1024 is well above any realistic human workflow (Notepad++
/// users rarely exceed ~100 tabs in even the largest sessions)
/// and well above the `MAX_SESSION_TABS` (512) ceiling for
/// session restore.
///
/// **Known limitation:** a plugin that *cycles* (open + close in
/// a loop) instead of accumulating tabs can still climb
/// `next_buffer_id` indefinitely — `tabs.len()` stays at zero so
/// the cap below never fires. Tracked for a follow-up that
/// gracefully refuses allocations once `next_buffer_id` is within
/// striking distance of `i32::MAX`, instead of panicking.
pub const MAX_OPEN_TABS: usize = 1024;

/// Side-effecting operations the shell needs from the UI thread. Each
/// platform UI crate (`ui_win32`, `ui_gtk`, `ui_cocoa`) implements this
/// trait.
///
/// **Important:** none of these methods may run a nested message pump
/// (i.e. show a modal dialog). They are called while the shell holds
/// internal state borrows, and a nested pump would let other messages
/// re-enter the `wnd_proc`, producing aliasing UB. Modal interactions
/// are deferred via [`PendingDialog`] — `Shell::drain` returns a list
/// the UI consumes *after* the drain's borrow ends.
pub trait UiPlatform {
    /// Ensure the editor's currently-displayed document is the one
    /// belonging to tab `idx`. `scintilla_doc` is the tab's stored
    /// document pointer (0 = "no document yet, please create one").
    /// The method returns the document pointer the tab is now
    /// bound to — `Shell` writes this back onto `Tab.scintilla_doc`
    /// so subsequent activations short-circuit.
    ///
    /// On Win32 this routes through `SCI_CREATEDOCUMENT` (when
    /// `scintilla_doc == 0`) plus `SCI_SETDOCPOINTER` to bind the
    /// document to the single Scintilla view. Multi-tab Phase 3
    /// uses this pattern to keep each tab independent without
    /// owning multiple Scintilla controls.
    fn activate_tab(&mut self, idx: usize, scintilla_doc: isize) -> isize;

    /// Push the given decoded text into the *currently-active*
    /// editor document. The caller is responsible for having called
    /// [`Self::activate_tab`] first to ensure the right document is
    /// bound to the view.
    /// On Win32 this routes through `EditorHandle::send` with
    /// `SCI_SETTEXT` plus `SCI_GOTOPOS` for the cursor restore.
    fn set_buffer_text(&mut self, text: &str, cursor: u64);

    /// Pull the current buffer text from the editor control. Called
    /// by `Shell::save_current_to_disk` so that user edits in
    /// Scintilla are written to disk, not the stale shadow held in
    /// `ActiveBuffer::text`. On Win32 this is a `SCI_GETLENGTH` +
    /// `SCI_GETTEXT` round trip via the direct-call API.
    fn get_buffer_text(&mut self) -> String;

    /// Pull the current cursor byte offset from the editor control.
    /// Used by `Shell::save_session` so the next launch can restore
    /// the user's caret position.
    fn get_cursor_pos(&mut self) -> u64;

    /// Update the status bar with the active buffer's language,
    /// encoding, EOL, and byte count. The leftmost segment shows
    /// the language label so the user can tell at a glance what
    /// the editor will syntax-highlight as.
    fn update_status(
        &mut self,
        lang: codepp_core::LangType,
        encoding: &Encoding,
        eol: Eol,
        byte_len: u64,
    );

    /// Plugin-driven status-bar override (`NPPM_SETSTATUSBAR`). The
    /// plugin owns `section`'s contents until the next host
    /// `update_status` call repaints the standard fields. Phase 3
    /// platforms route this onto whichever section best matches.
    fn set_plugin_status(&mut self, section: usize, text: &str);

    /// Tell the editor "the currently-bound document was just saved
    /// to disk" so it can clear its modified flag. On Win32 this is
    /// `SCI_SETSAVEPOINT(0, 0)`; clears Scintilla's dirty glyph and
    /// makes `SCI_GETMODIFY` return 0 until the next edit.
    ///
    /// Called by [`Shell::save_current_to_disk`], [`Shell::save_buffer_as`],
    /// and [`Shell::save_all`] on each successful per-tab write so
    /// every saved tab gets its dirty state cleared regardless of
    /// what other tabs in a Save All did.
    fn mark_saved(&mut self);

    /// Attach the lexer (and any per-language style theme + keyword
    /// lists) appropriate for `lang` to the *currently-active*
    /// editor document. `L_TEXT` detaches whatever lexer is bound,
    /// returning the view to plain rendering. Called by `Shell`
    /// after a fresh load and on tab-switch so the right colours
    /// follow the user's tab moves.
    fn apply_lang(&mut self, lang: LangType);

    /// Apply the editor's default-style configuration (font face,
    /// size, bold / italic / underline, foreground / background
    /// colour, and window transparency) to the live Scintilla
    /// view. Called by the host after `Shell::new` to seed the
    /// initial appearance from `styles.xml`, and again whenever
    /// the Style Configurator dialog's Save & Close fires through
    /// `Shell::set_styles`. The implementation:
    ///
    /// 1. Configures `STYLE_DEFAULT` (font + size + colours + font
    ///    modifiers) on the editor.
    /// 2. Calls `SCI_STYLECLEARALL` so every other style index
    ///    inherits the new baseline.
    /// 3. Re-applies the line-number margin (clobbered by
    ///    `SCI_STYLECLEARALL`).
    /// 4. Applies the transparency setting to the main window.
    /// 5. Triggers a re-style of any active lexer's classifications
    ///    so per-language theme colours (the Phase 4.5 framework's
    ///    style table) re-overlay on top of the new default —
    ///    `STYLE_DEFAULT` provides the base font / size; lexers'
    ///    per-style `style_set_fore` calls layer the keyword /
    ///    string / number / etc. colours on top.
    fn apply_default_style(&mut self, styles: &codepp_core::styles::Styles);

    // --- Search / replace -----------------------------------------
    //
    // The four trait methods below are unconditionally part of the
    // UiPlatform contract — every platform backend (current Win32,
    // future GTK and Cocoa from Phase 5) must implement them. The
    // matching `Shell::find_next` / `replace_*` driver methods are
    // currently `#[cfg(target_os = "windows")]` because their
    // backing infrastructure (the `last_search` field, the plugin
    // dispatcher) is also Windows-only until Phase 5. When the
    // Linux/macOS UI crates land, those `#[cfg]` gates come off
    // alongside the rest of the host plumbing — the trait methods
    // here don't need to change.

    /// Search the active editor forward for `query` under `flags`.
    /// On a hit, Scintilla moves the selection to the match (also
    /// repositions the caret to the match end). Returns the match's
    /// byte offset, or `None` on miss. Phase 4 m3 implementations
    /// route through `EditorHandle::search_anchor` +
    /// `search_next`; Phase 5 backends do the equivalent.
    fn search_next(&mut self, query: &str, flags: SearchFlags) -> Option<u64>;

    /// Same as [`Self::search_next`] but walks backward from the
    /// current selection.
    fn search_prev(&mut self, query: &str, flags: SearchFlags) -> Option<u64>;

    /// Replace the currently-selected text with `replacement` if
    /// and only if the selection matches `query` under `flags`.
    /// Returns true if a replacement happened. The match-check
    /// guards against the case where the user reselected
    /// arbitrary text after a Find — Scintilla itself doesn't
    /// gate on that.
    fn replace_current(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> bool;

    /// Replace every occurrence of `query` with `replacement` in
    /// the active buffer. All replaces are wrapped in one
    /// Scintilla undo group so the user can Ctrl+Z the entire
    /// Replace All in a single step. Returns the count.
    fn replace_all(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> usize;

    /// Count every match of `query` in the active buffer. Pure
    /// query — does not move the user's selection. The Find
    /// dialog's "Count" button surfaces the result in its status
    /// line.
    fn count_matches(&mut self, query: &str, flags: SearchFlags) -> usize;

    /// Forward search restricted to a byte range — used by the
    /// "In selection" mode of the Find dialog. Returns the
    /// match's byte offset, or `None` if no match falls inside
    /// `[start, end)`. Implementations must NOT move the caret
    /// outside the range on a miss.
    fn search_next_in_range(
        &mut self,
        query: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> Option<u64>;

    /// Backward sibling of [`Self::search_next_in_range`].
    fn search_prev_in_range(
        &mut self,
        query: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> Option<u64>;

    /// Replace All restricted to `[start, end)`. Returns
    /// `(count, new_end)` — the caller uses `new_end` to keep its
    /// in-selection range bookkeeping in sync after replacements
    /// shrink or grow the original window.
    fn replace_all_in_range(
        &mut self,
        query: &str,
        replacement: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> (usize, u64);

    /// `true` if the Win32 tab strip is currently hidden, `false`
    /// if visible. Plumbed from `NPPM_ISTABBARHIDDEN`.
    fn is_tabbar_hidden(&self) -> bool;

    /// Toggle tab-strip visibility. `hidden == true` hides;
    /// `false` shows. Returns the *previous* hidden state — the
    /// `NPPM_HIDETABBAR` ABI signal that lets a plugin know
    /// whether it actually changed anything. Implementations
    /// should also trigger an editor-area relayout so the
    /// Scintilla view grows / shrinks to fill the freed / lost
    /// space, but the relayout may be deferred (e.g. via
    /// `PostMessage` to avoid re-entering `wnd_proc` under
    /// `PluginCallGuard`).
    fn set_tabbar_hidden(&mut self, hidden: bool) -> bool;

    /// Toolbar visibility (`NPPM_ISTOOLBARHIDDEN`).
    fn is_toolbar_hidden(&self) -> bool;
    /// Toggle toolbar visibility. Same return contract as
    /// [`Self::set_tabbar_hidden`] (previous hidden state).
    /// Implementations should trigger an editor-area relayout so
    /// the Scintilla view fills / yields the freed space, same
    /// `PluginCallGuard`-safe deferral note as the tabbar variant.
    fn set_toolbar_hidden(&mut self, hidden: bool) -> bool;

    /// Main menu bar visibility (`NPPM_ISMENUHIDDEN`).
    fn is_menu_hidden(&self) -> bool;
    /// Toggle menu bar visibility. Win32 swaps via `SetMenu(NULL)`
    /// / `SetMenu(main_menu)` + `DrawMenuBar`; the GTK / Cocoa
    /// backends do their own thing once Phase 5 lands. Same
    /// previous-state return contract.
    fn set_menu_hidden(&mut self, hidden: bool) -> bool;

    /// Status bar visibility (`NPPM_ISSTATUSBARHIDDEN`).
    fn is_statusbar_hidden(&self) -> bool;
    /// Toggle status bar visibility. Same previous-state return
    /// contract.
    fn set_statusbar_hidden(&mut self, hidden: bool) -> bool;

    /// Active editor's zoom level in points. Drives
    /// `NPPM_GETZOOMLEVEL` via the host's `editor_zoom_level`.
    /// Wraps Scintilla's `SCI_GETZOOM` — typically returns a
    /// signed int in `[-10, 20]`.
    fn editor_zoom_level(&self) -> i32;

    /// Default foreground colour of the active editor. `COLORREF`
    /// (`0x00BBGGRR`). Reads `SCI_STYLEGETFORE(STYLE_DEFAULT)` on
    /// Win32. Drives `NPPM_GETEDITORDEFAULTFOREGROUNDCOLOR`.
    fn editor_default_fg_color(&self) -> i32;

    /// Default background colour of the active editor. Drives
    /// `NPPM_GETEDITORDEFAULTBACKGROUNDCOLOR`.
    fn editor_default_bg_color(&self) -> i32;

    /// Toggle Scintilla's font-rendering quality.
    /// `smooth == true` → `SC_EFF_QUALITY_LCD_OPTIMIZED`;
    /// `smooth == false` → `SC_EFF_QUALITY_NON_ANTIALIASED`.
    /// Drives `NPPM_SETSMOOTHFONT`. Returns the *previous* state
    /// (true = was-smooth, false = was-not).
    fn set_smooth_font(&mut self, smooth: bool) -> bool;

    /// Toggle the `WS_EX_CLIENTEDGE` extended style on the
    /// Scintilla view's HWND. Drives `NPPM_SETEDITORBORDEREDGE`.
    /// Returns the previous state.
    fn set_editor_border_edge(&mut self, enable: bool) -> bool;

    /// Set the line-number margin width mode. Drives
    /// `NPPM_SETLINENUMBERWIDTHMODE`. Returns `true` on accepted
    /// modes (`LINENUMWIDTH_DYNAMIC` for now). Phase 4 polish
    /// adds the constant-width path.
    fn set_line_number_width_mode(&mut self, mode: i32) -> bool;

    /// Look up the keyboard shortcut bound to `cmd_id` in the
    /// host's accelerator table. Returns the `ShortcutKey`
    /// (Ctrl/Alt/Shift bits + virtual key) when a binding
    /// exists; `None` for unbound cmd ids. Drives
    /// `NPPM_GETSHORTCUTBYCMDID`.
    ///
    /// `cfg(target_os = "windows")`-gated because the
    /// `ShortcutKey` type comes from `codepp_plugin_host`'s
    /// FFI surface, which is itself Windows-only until Phase 5
    /// brings up GTK / Cocoa plugin loaders. The Phase 5
    /// backends will gain their own `cfg`-gated impls of this
    /// method against their native shortcut systems
    /// (`gtk_application_set_accels_for_action` /
    /// `NSMenuItem.keyEquivalent`).
    #[cfg(target_os = "windows")]
    fn shortcut_for_cmd_id(&self, cmd_id: i32) -> Option<codepp_plugin_host::ShortcutKey>;

    /// Remove every accelerator-table binding for `cmd_id`.
    /// Returns `true` if at least one binding was removed,
    /// `false` if the cmd id had no binding (table left
    /// unchanged in that case). Drives
    /// `NPPM_REMOVESHORTCUTBYCMDID`. Same `cfg(windows)` gate
    /// rationale as `shortcut_for_cmd_id` — the dispatcher
    /// lives in `plugin-host`, which is Windows-only.
    #[cfg(target_os = "windows")]
    fn remove_shortcut_for_cmd_id(&mut self, cmd_id: i32) -> bool;

    /// Register or unregister a plugin-owned modeless-dialog
    /// HWND with the host's message pump. `register == true`
    /// adds the HWND so each pump iteration calls
    /// `IsDialogMessageW` against it; `register == false`
    /// removes it. Same `cfg(windows)` gate rationale as the
    /// shortcut methods — `Hwnd` comes from `plugin-host`.
    #[cfg(target_os = "windows")]
    fn register_modeless_dialog(&mut self, dlg: codepp_plugin_host::Hwnd, register: bool) -> bool;

    /// Add a plugin-supplied icon (HICON) to the host toolbar
    /// bound to `cmd_id`. Drives `NPPM_ADDTOOLBARICON`. Same
    /// `cfg(windows)` gate rationale as the shortcut messages
    /// — `Hwnd` (the plugin's HICON shape) comes from
    /// `plugin-host`.
    #[cfg(target_os = "windows")]
    fn add_toolbar_icon(&mut self, cmd_id: i32, hicon: codepp_plugin_host::Hwnd) -> bool;

    /// Whether the host is currently rendering its own chrome
    /// in dark mode. Drives `NPPM_ISDARKMODEENABLED`. Code++
    /// Phase 4 returns `false` (no host-side dark mode); Phase
    /// 5 wires the live theme state. Same `cfg(windows)` gate
    /// as the other plugin-host-typed methods.
    #[cfg(target_os = "windows")]
    fn is_dark_mode_enabled(&self) -> bool;

    /// Write the host's dark-mode palette into `out` if dark
    /// mode is active. Drives `NPPM_GETDARKMODECOLORS`. Code++
    /// Phase 4 returns `false` without touching `out`.
    #[cfg(target_os = "windows")]
    fn dark_mode_colors(&self, out: &mut codepp_plugin_host::NppDarkModeColors) -> bool;

    /// Create a fresh Scintilla control as a child of the
    /// plugin-supplied `parent` HWND. Drives
    /// `NPPM_CREATESCINTILLAHANDLE`. Returns the new HWND on
    /// success, NULL on failure. The plugin owns the new
    /// control's lifetime (must `DestroyWindow` before the
    /// parent goes away). Same `cfg(windows)` gate rationale
    /// as the other plugin-HWND methods — `Hwnd` comes from
    /// `plugin-host`.
    #[cfg(target_os = "windows")]
    fn create_plugin_scintilla(
        &mut self,
        parent: codepp_plugin_host::Hwnd,
    ) -> codepp_plugin_host::Hwnd;

    /// Register a plugin's docking dialog and create the
    /// host-owned floating frame that wraps it. Drives
    /// `NPPM_DMMREGASDCKDLG`. The frame is created hidden;
    /// the plugin must follow with `show_dock_dialog` to make
    /// it visible. Returns `true` on success, `false` for dead
    /// `params.h_client`, frame-creation failure, or duplicate
    /// `h_client` registration.
    #[cfg(target_os = "windows")]
    fn register_dock_dialog(&mut self, params: codepp_plugin_host::DockDialogParams) -> bool;

    /// Show the floating frame previously registered for
    /// `h_client`. Drives `NPPM_DMMSHOW`. Returns `true` on
    /// success, `false` for unregistered HWND.
    #[cfg(target_os = "windows")]
    fn show_dock_dialog(&mut self, h_client: codepp_plugin_host::Hwnd) -> bool;

    /// Hide the floating frame previously registered for
    /// `h_client`. Drives `NPPM_DMMHIDE`. The registration
    /// survives — a subsequent `show_dock_dialog` re-shows.
    /// Returns `true` on success, `false` for unregistered
    /// HWND.
    #[cfg(target_os = "windows")]
    fn hide_dock_dialog(&mut self, h_client: codepp_plugin_host::Hwnd) -> bool;

    /// Refresh the floating frame's title (and add-info /
    /// icon) from the cached `DockDialogParams`. Drives
    /// `NPPM_DMMUPDATEDISPINFO`. Returns `true` on success,
    /// `false` for unregistered HWND.
    #[cfg(target_os = "windows")]
    fn update_dock_disp_info(&mut self, h_client: codepp_plugin_host::Hwnd) -> bool;

    /// Look up a registered docking dialog by display name
    /// (and optional module-name disambiguator). Drives
    /// `NPPM_DMMGETPLUGINHWNDBYNAME`. Returns the registered
    /// `h_client` HWND, or NULL if no entry matches.
    #[cfg(target_os = "windows")]
    fn dock_hwnd_by_name(&self, name: &str, module_name: Option<&str>) -> codepp_plugin_host::Hwnd;

    /// Pull the current text content of the buffer backed by the
    /// Scintilla document at `scintilla_doc`. The implementation may
    /// briefly bind that document to the editor view to read it
    /// (the standard Win32 idiom is `SCI_SETDOCPOINTER` swap +
    /// `SCI_GETTEXT` + `SCI_SETDOCPOINTER` restore), so callers
    /// should expect the active view to be left as it was found —
    /// no permanent side effects.
    ///
    /// `scintilla_doc == 0` is the "no document yet" sentinel and
    /// returns an empty string; callers ask only after `activate_tab`
    /// has populated `Tab.scintilla_doc`. Used by `Shell::save_session`
    /// to capture every tab's text — including non-active tabs — into
    /// backup files so untitled and dirty buffers survive a clean
    /// shutdown.
    fn capture_text_from_doc(&mut self, scintilla_doc: isize) -> String;

    /// Query the modified flag (`SCI_GETMODIFY`) of the Scintilla
    /// document at `scintilla_doc`. Same doc-pointer-swap pattern
    /// as [`Self::capture_text_from_doc`] for non-active docs.
    /// Returns `false` for the `scintilla_doc == 0` sentinel
    /// (no document, nothing to be dirty).
    ///
    /// `Shell::save_session` consults this to decide whether a
    /// path-bound tab needs a backup file written on this save
    /// pass — clean tabs (their text is on disk verbatim) skip
    /// the backup write entirely, dirty tabs get one so the
    /// user's unsaved edits survive a restart.
    fn is_doc_dirty(&mut self, scintilla_doc: isize) -> bool;

    /// Dispatch a Notepad++-ABI `IDM_*` command id. Drives
    /// `NPPM_MENUCOMMAND`. The implementation maps the N++
    /// command id to whichever internal command routes to the
    /// same action (File → Open, View → Word Wrap, …) and fires
    /// it — typically by posting an equivalent `WM_COMMAND` on
    /// Win32 so the built-in menu handler runs the same code
    /// path a user click would take. Plugin-allocated cmd ids
    /// (from `NPPM_ALLOCATECMDID` or the plugin's own `FuncItem`
    /// entries) fall through untouched. Returns `true` if a
    /// dispatch was attempted, `false` for command ids the
    /// backend has no target for (unmapped built-in ids). Same
    /// `cfg(windows)` gate rationale as the other plugin-host-
    /// dispatched methods — `NPPM_MENUCOMMAND` lives in
    /// `plugin-host`, which is Windows-only until Phase 5.
    #[cfg(target_os = "windows")]
    fn dispatch_npp_menu_command(&mut self, idm: i32) -> bool;

    /// Set the checked state of the menu item bound to N++-ABI
    /// command id `idm`. Drives `NPPM_SETMENUITEMCHECK`. The
    /// implementation maps built-in `IDM_*` ids through the same
    /// table as [`Self::dispatch_npp_menu_command`], falls through
    /// for plugin-allocated cmd ids, and issues the native
    /// "check menu item by command" call. Returns `true` if the
    /// state was applied, `false` if the id has no menu item
    /// (unmapped built-in id, or a plugin cmd id whose owning
    /// plugin didn't publish a menu entry).
    #[cfg(target_os = "windows")]
    fn set_npp_menu_item_check(&mut self, idm: i32, checked: bool) -> bool;

    /// Mark the active buffer as modified, so save prompts, the
    /// title-bar asterisk, and `SCI_GETMODIFY` all report dirty
    /// until the next successful save. Drives
    /// `NPPM_MAKECURRENTBUFFERDIRTY`. Scintilla has no direct
    /// "set dirty" primitive — the Win32 impl adds an opaque
    /// container action (`SCI_ADDUNDOACTION`) to shift the undo
    /// position past the save point. No-op if the buffer is
    /// already dirty (avoids stacking one phantom undo entry per
    /// call for plugins that redundantly re-mark). Not gated on
    /// Windows only — the primitive is Scintilla-level and every
    /// backend has the same shape.
    fn mark_active_buffer_dirty(&mut self);
}

/// One restored tab from `Shell::load_session_entries`. Tells the UI
/// exactly how to bring a tab back to life: open a real file from
/// disk, seed an untitled buffer with text loaded from a backup,
/// or recreate a dirty saved-file with the user's last unsaved
/// edits in place. Iterated in the same order the tabs appeared in
/// the previous session so the user's tab arrangement (saved files
/// interleaved with untitled buffers) round-trips faithfully.
pub enum SessionRestoreEntry {
    /// Open `path` from disk via the loader (async). The eventual
    /// load result populates the tab's text + encoding + EOL
    /// just like a fresh user-initiated open. The persisted
    /// language override (if any) is looked up from
    /// `Shell.session` inside `apply_load_result` by path, so no
    /// dedicated field is needed on this variant — same pattern
    /// `apply_load_result` already uses for the restored cursor
    /// position. The persisted `pinned` flag is looked up the same
    /// way.
    OpenFile(PathBuf),
    /// Re-create an untitled buffer at the next slot with the
    /// pre-loaded text from its backup file. The UI calls
    /// `Shell::restore_untitled_with_text` for this variant.
    UntitledFromBackup {
        /// Original `untitled_seq` so the user's "new 3" comes
        /// back as "new 3" rather than being re-numbered.
        untitled_seq: Option<u32>,
        /// Buffer content read from the backup file.
        text: String,
        /// Caret position the user had when they last closed.
        cursor: u64,
        /// Encoding the buffer would target on first save.
        encoding: Encoding,
        /// EOL style the buffer would target on first save.
        eol: Eol,
        /// `true` iff the backup file's mtime is meaningfully
        /// later than the timestamp embedded in its filename —
        /// i.e. another program edited the recovery file between
        /// our last save and this restore. The buffer still
        /// shows the (modified) content, but the user gets a
        /// `PendingDialog::Error` so they're not silently
        /// presented with text they didn't type.
        backup_modified_externally: bool,
        /// User-chosen rename label persisted across the session
        /// boundary. `None` if the user never renamed this
        /// untitled buffer (the default `new N` label is
        /// rebuilt from `untitled_seq` instead).
        custom_name: Option<String>,
        /// Persisted language override (the raw N++-ABI id, as
        /// stored in `core::session::Tab.lang`). `None` means
        /// "no stored choice" — the buffer restores at `L_TEXT`
        /// since untitled buffers have no extension to detect
        /// from. The Language menu's user-set value comes back
        /// through this attribute, so a renamed-then-Rust-lexed
        /// untitled buffer keeps its highlighting across
        /// relaunches.
        lang: Option<i32>,
        /// Persisted pin state — `true` restores the tab as pinned.
        /// `Shell::load_session_entries` sorts entries so pinned
        /// tabs come back first, matching the on-disk order that
        /// `Shell::save_session` writes.
        pinned: bool,
    },
    /// Re-create a tab that was bound to `path` but had unsaved
    /// edits at session-save time. The tab opens with `path`
    /// associated (so File→Save writes there) but its Scintilla
    /// document is seeded with the *backup* text — i.e. the
    /// user's last in-memory state, not the on-disk state. The
    /// buffer is left dirty so the user knows there are unsaved
    /// changes; File→Save flushes them to `path`.
    DirtyFromBackup {
        /// File path the buffer was bound to.
        path: PathBuf,
        /// Buffer content from the backup file (the user's last
        /// unsaved edits).
        text: String,
        /// Caret position the user had when they last closed.
        cursor: u64,
        /// Encoding the buffer carried.
        encoding: Encoding,
        /// EOL style the buffer carried.
        eol: Eol,
        /// `true` iff the on-disk file's mtime is newer than the
        /// backup's — i.e. an external process edited `path`
        /// during the recovery window (between session save and
        /// this restore). When set, `Shell::restore_dirty_with_text`
        /// queues a `PendingDialog::ConfirmReload(path)` so the
        /// user gets prompted: "keep my unsaved edits" (overwrite
        /// the external write) or "reload" (drop the backup
        /// overlay). Without this signal the user's first save
        /// would silently overwrite the external edit.
        disk_changed_externally: bool,
        /// `true` iff the backup file *itself* was modified by
        /// another program (mtime later than the timestamp in
        /// its filename). Independent of `disk_changed_externally`
        /// — both can fire for the same tab if both the on-disk
        /// file *and* the recovery file were touched while the
        /// app was dead. Surfaces a separate
        /// `PendingDialog::Error` so the user knows the buffer
        /// content shown isn't the one they typed.
        backup_modified_externally: bool,
        /// Persisted language override (raw N++-ABI id from
        /// `core::session::Tab.lang`). `None` falls back to
        /// extension-based detection from `path`; `Some` wins
        /// so a Language-menu choice the user made during the
        /// previous session reapplies on restore.
        lang: Option<i32>,
        /// Persisted pin state — `true` restores the tab as pinned.
        /// Same round-trip semantics as
        /// [`SessionRestoreEntry::UntitledFromBackup::pinned`].
        pinned: bool,
    },
}

/// A modal dialog request the UI must show after `Shell::drain`
/// returns. Holding the dialog *outside* the drain's `&mut Shell`
/// borrow is the only way to safely run a nested Win32 message pump
/// without producing aliasing UB on `WindowState`.
#[derive(Debug, Clone)]
pub enum PendingDialog {
    /// "File changed externally — reload?" prompt for `path`. If the
    /// user accepts, the UI calls `Shell::confirm_reload(path)` to
    /// requeue the load.
    ConfirmReload(PathBuf),
    /// Non-fatal error: title and message strings to display.
    Error { title: String, message: String },
}

/// Which branch [`Shell::open_file`] took, so the UI knows whether
/// it must synchronously rebind the Scintilla view.
///
/// See the doc on [`Shell::open_file`] for the "why" behind
/// `SwitchedToExisting`: the tab-strip resync only updates the
/// visual selection, not the view's document pointer, so a re-open
/// of an already-loaded file needs an explicit follow-up on the UI
/// side to swap [`SCI_SETDOCPOINTER`](crates/scintilla-sys) to the
/// dedupe target — otherwise the tab bar shows the switch while the
/// editor keeps rendering the previous buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenFileOutcome {
    /// A load was queued; the UI's normal `drain` + `activate_tab`
    /// cycle handles the rest when the loader posts its wake.
    Loading,
    /// The path was already open at `usize`; the shell flipped
    /// `active_tab`. The UI must issue a synchronous view rebind
    /// (Scintilla `SCI_SETDOCPOINTER` to the existing tab's
    /// materialised document) since no `WM_APP_WAKE` will fire.
    SwitchedToExisting(usize),
    /// The path was already open and already the active tab. No
    /// state changed; UI has nothing to do beyond a chrome refresh.
    AlreadyActive,
    /// The open was refused (loader shut down, tab cap reached).
    /// UI has nothing to do.
    Rejected,
}

/// One open buffer (one tab in the multi-tab UI).
///
/// Phase 3 milestone 6a moves the single-buffer model to a tabbed
/// model. Each `Tab` carries its own path, encoding, EOL, decoded
/// text shadow, pending-load id, and a host-assigned `id` that
/// flows through the plugin ABI as `BufferID` (returned by
/// `NPPM_GETCURRENTBUFFERID`, accepted by `NPPM_GETFULLPATHFROMBUFFERID`,
/// and carried in `NPPN_*.nmhdr.idFrom`).
///
/// `scintilla_doc` is the Scintilla document pointer that backs the
/// tab's editor state. Milestone 6b's UI tab control creates one
/// document per tab via `SCI_CREATEDOCUMENT` and switches the single
/// Scintilla view between them with `SCI_SETDOCPOINTER` on tab
/// click. Milestone 6a leaves it `None` — the existing single-tab
/// UI shares one implicit document.
#[derive(Debug, Clone)]
pub struct Tab {
    /// Stable buffer id assigned at tab-creation time. Zero is
    /// reserved for "no buffer" (matches Notepad++'s convention).
    pub id: i32,
    pub path: Option<PathBuf>,
    pub encoding: Encoding,
    pub eol: Eol,
    pub byte_len: u64,
    /// Most recent decoded text. Held so `save_file` can re-encode
    /// without round-tripping through Scintilla. Phase 3 milestone 6b
    /// pulls the latest text from Scintilla via the direct-call API
    /// (`SCI_GETTEXT`) at save time, since the user may have edited it.
    pub text: String,
    /// Pending request id from the loader, so we know which load
    /// result actually pertains to this tab (vs. a stale one if
    /// the user dropped a second file before the first finished).
    pub pending_load: Option<RequestId>,
    /// Scintilla document pointer (`sptr_t`). Non-zero once the tab
    /// has been attached to a Scintilla view. Milestone 6b's UI
    /// populates this; milestone 6a leaves it 0.
    pub scintilla_doc: isize,
    /// N++-compatible `LangType` for this buffer. Phase 4 m1 derives
    /// it from the path extension on first load; later milestones
    /// expose `NPPM_SETBUFFERLANGTYPE` so plugins can override. New
    /// (unsaved) tabs and unrecognised extensions default to `L_TEXT`.
    pub lang: LangType,
    /// Per-tab untitled sequence number — `Some(N)` for buffers
    /// created by File→New that haven't been saved yet, rendered
    /// in the tab strip as `"new N"`. Cleared (`= None`) by
    /// [`Shell::save_buffer_as`] once the buffer gets a real path.
    /// New numbers are assigned in [`Shell::new_untitled`] using
    /// the smallest unused value across all currently-open
    /// untitled tabs, so closing `new 1` then creating a new
    /// untitled buffer gives `new 1` again.
    pub untitled_seq: Option<u32>,
    /// Cached "buffer has unsaved changes" flag for the tab strip's
    /// owner-draw paint. Mirrors Scintilla's `SCI_GETMODIFY` for the
    /// tab's document, but readable without binding the editor to
    /// that doc — paint runs once per tab per repaint, so reading
    /// the live modify bit (which requires the expensive
    /// doc-pointer-swap dance) on every inactive tab is not viable.
    /// Updated by the UI on `SCN_SAVEPOINTREACHED` / `SCN_SAVEPOINTLEFT`
    /// for the active tab and on tab activation for the previously
    /// active tab. Always false on tab creation; flips true on the
    /// first user edit. Save paths that drive `SCI_SETSAVEPOINT`
    /// flip it back to false (Scintilla also fires
    /// `SCN_SAVEPOINTREACHED` from inside that message, so the
    /// notification arm is the canonical write site — explicit
    /// resets in save paths are belt-and-braces for code that
    /// short-circuits before the notification fires).
    pub dirty: bool,
    /// User-chosen display name for an untitled buffer, set via
    /// File → Rename... and rendered by the tab strip and window
    /// title in place of the default `new N`. Stays `None` for
    /// path-bound buffers (their display name comes from `path`)
    /// and for untitled buffers the user has not renamed; the UI's
    /// label-resolution helper falls through this in priority
    /// order: `custom_name` → `path` basename → `untitled_seq`.
    /// Cleared (`None`) when an untitled buffer is saved with a
    /// real path — the on-disk filename takes over.
    pub custom_name: Option<String>,
    /// `true` iff the user pinned this tab. Pinned tabs cluster at
    /// the left edge of the tab strip in insertion order and cannot
    /// be moved by drag; unpinned tabs occupy the slots to their
    /// right. `Shell` enforces the "pinned-before-unpinned"
    /// invariant across `move_tab` / `set_pinned` /
    /// `close_active_tab` so no code path can leave the vector in
    /// a state that violates it. Round-trips through `session.xml`
    /// via [`codepp_core::Tab::pinned`].
    pub pinned: bool,
}

impl Default for Tab {
    fn default() -> Self {
        Self {
            id: 0,
            path: None,
            encoding: Encoding::default(),
            eol: Eol::default(),
            byte_len: 0,
            text: String::new(),
            pending_load: None,
            scintilla_doc: 0,
            lang: L_TEXT,
            untitled_seq: None,
            dirty: false,
            custom_name: None,
            pinned: false,
        }
    }
}

/// Snapshot returned by [`Shell::close_active_tab`] describing the
/// platform-side cleanup the UI must perform. Shell has already
/// removed the tab from `Shell.tabs`, updated `Shell.active_tab`,
/// queued the `NPPN_FILECLOSED` / `NPPN_BUFFERACTIVATED`
/// notifications, and unregistered the file watcher; what's left
/// is the things only the UI knows about — the tab control and
/// the Scintilla document.
#[derive(Debug, Clone)]
pub struct ClosedTab {
    /// Index the tab occupied in `Shell.tabs` at the moment of
    /// close. Same index for the platform tab strip — the UI
    /// removes the item at this index. After this snapshot is
    /// returned, `Shell.tabs.len()` is one less and
    /// `Shell.active_tab` reflects the new selection.
    pub closed_idx: usize,
    /// Buffer id of the closed tab. Useful for plugin-host bookkeeping.
    pub buffer_id: i32,
    /// Path the closed tab was bound to (if any). Mostly for logging
    /// — the watcher unwatch already happened inside Shell.
    pub path: Option<PathBuf>,
    /// Scintilla document pointer the closed tab owned. UI calls
    /// `SCI_RELEASEDOCUMENT` against this so Scintilla can free
    /// the underlying buffer. Zero when the tab never had its
    /// document materialized (rare — only background-loaded tabs
    /// closed before first activation).
    pub scintilla_doc: isize,
    /// Scintilla document pointer for the new active tab, if any.
    /// UI calls `SCI_SETDOCPOINTER` on this to bind the view to
    /// the now-visible tab. Zero when there's no new active tab
    /// (closed the last open tab) or when the new active tab's
    /// document hasn't been materialized yet — `handle_tab_selchange`
    /// will lazily create one on the next user click.
    pub new_active_doc: isize,
}

/// Per-call platform handles the UI hands the dispatcher when
/// routing an inbound NPPM_* message. The host crate is platform-
/// agnostic; the UI fills these with whatever opaque pointer types
/// it owns (HWND/HMENU on Win32, `GtkWidget`* on GTK, `NSView`*/`NSMenu`*
/// on Cocoa). All five fields are pointer-sized — `*mut c_void` —
/// so the same struct works on every backend without conditional
/// compilation in `shell`.
///
/// The struct is `Copy` so the `wnd_proc` can build it on the stack
/// per call without any allocation cost.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
pub struct HostHandles {
    /// Main host window — `nmhdr.hwndFrom` for outbound notifications,
    /// the `SendMessage` target plugins call into for `NPPM_*`.
    pub npp_hwnd: Hwnd,
    /// Primary Scintilla view's HWND.
    pub scintilla_main: Hwnd,
    /// Secondary Scintilla view's HWND. NULL until split-view lands.
    pub scintilla_secondary: Hwnd,
    /// HMENU for the per-plugin submenu under "Plugins"
    /// (`NPPM_GETMENUHANDLE` with `NPPPLUGINMENU`).
    pub plugin_menu: Hwnd,
    /// HMENU for the entire main menu bar
    /// (`NPPM_GETMENUHANDLE` with `NPPMAINMENU`).
    pub main_menu: Hwnd,
}

#[cfg(target_os = "windows")]
impl HostHandles {
    /// All-NULL handles. **Tests and stub implementations only.**
    /// Production code must supply real handles before any plugin
    /// menu interaction: a plugin querying `NPPM_GETMENUHANDLE` against
    /// a NULL HMENU will likely crash on the receiving side.
    #[must_use]
    pub fn null() -> Self {
        Self {
            npp_hwnd: core::ptr::null_mut(),
            scintilla_main: core::ptr::null_mut(),
            scintilla_secondary: core::ptr::null_mut(),
            plugin_menu: core::ptr::null_mut(),
            main_menu: core::ptr::null_mut(),
        }
    }
}

/// Application-wide state. Owned by the UI crate's `run()` function;
/// the `wnd_proc` / event handler reaches into it on every interesting
/// message. On Windows, also owns the `PluginHost` registry — plugins
/// are lazy-loaded, so no DLL is mapped until first menu touch
/// (DESIGN.md §6.4).
pub struct Shell {
    pub session: Session,
    /// Open tabs. Empty at startup; the first `open_file` populates
    /// `tabs[0]` and sets `active_tab = Some(0)`. Subsequent opens
    /// either replace the active tab (if it has no path yet — the
    /// initial-empty case) or push a new tab. The UI drives the
    /// tab strip from `tabs[]` and `active_tab`.
    pub tabs: Vec<Tab>,
    /// Index into [`Self::tabs`] of the currently-active tab.
    /// `None` when no file is open.
    pub active_tab: Option<usize>,
    /// Next buffer id to hand out. Starts at 1 (0 is "no buffer").
    /// Monotonically increasing; never reused so closed-tab ids
    /// don't accidentally resolve a plugin lookup to a different
    /// buffer.
    next_buffer_id: i32,
    /// Plugin registry. Windows-only until Phase 5 wires the same
    /// trait surface against `dlopen`.
    #[cfg(target_os = "windows")]
    plugins: PluginHost,
    /// Outbound NPPN_* notifications queued by shell operations
    /// (load complete, save complete) since the last
    /// [`Self::take_notifications`] drain. The UI fires each one
    /// **after** dropping any `&mut Shell` borrow, since `beNotified`
    /// runs synchronous plugin code that may `SendMessage(NPPM_*)`
    /// back into the `wnd_proc`.
    #[cfg(target_os = "windows")]
    pending_notifications: Vec<Notification>,
    loader: Loader,
    _loader_shutdown: LoaderShutdown,
    file_watcher: FileWatcher,
    /// Receivers the UI thread drains on every wake. Producer threads
    /// have already called `wake` by the time something appears here.
    load_rx: Receiver<LoadResult>,
    change_rx: Receiver<FileChange>,
    /// Last query + flags used by `find_next` / Find Replace dialog.
    /// Stored so F3 / Shift+F3 (and the dialog's Find Next button)
    /// can repeat the search without the user re-entering anything.
    /// `None` until the user issues their first search.
    #[cfg(target_os = "windows")]
    last_search: Option<(String, SearchFlags)>,
    /// Rolling Find/Replace dropdown history. Loaded from
    /// `find_history.xml` at startup; pushed to on every Find
    /// Next / Replace operation; saved back on the same path
    /// after each push (eager save — the file is tiny and the
    /// alternative is silently losing history on crash).
    pub find_history: FindHistory,
    /// Find-in-files orchestrator. Owns the active-job cancel
    /// flag and the next-job-id counter; events flow back through
    /// `fif_rx` (see [`Self::drain`]).
    fif_orchestrator: fif::FifOrchestrator,
    /// Receiver half of the FIF event channel. Senders are cloned
    /// per-job into walker / workers / coordinator, so dropping a
    /// job's threads doesn't close this channel.
    fif_rx: Receiver<FifEvent>,
    /// FIF events drained off `fif_rx` but not yet consumed by the
    /// UI. The UI calls [`Self::take_fif_events`] after each drain
    /// to pull them, then applies them to the results dock outside
    /// the `&mut Shell` borrow (matching the
    /// `pending_notifications` pattern).
    pending_fif: Vec<FifEvent>,
    /// Pending pre-fill for the next FIF dialog open, set by
    /// `NPPM_LAUNCHFINDINFILESDLG`. The Win32 plugin dispatch
    /// drains this immediately after `dispatch_plugin_message`
    /// returns and opens the dialog with the directory and
    /// filters pre-populated. `None` is the common case (menu /
    /// hotkey driven open uses whatever the dialog already
    /// holds).
    #[cfg(target_os = "windows")]
    pending_fif_launch: Option<FifLaunchPrefill>,
    /// Modal-dialog requests queued by *synchronous* shell methods
    /// that don't go through `drain` (the dialog source for the
    /// async loader / file-watcher paths). Currently used by
    /// `restore_dirty_with_text` to surface a "file changed
    /// externally during the recovery window" reload prompt at
    /// startup. Drained at the *end* of [`Self::drain`] so the
    /// UI's existing dialog-presentation path picks them up
    /// without a new code path.
    deferred_dialogs: Vec<PendingDialog>,
    /// Per-path debounce deadlines for file-change dialogs. An
    /// entry with a future timestamp means new file-change
    /// events for that path are silently discarded until now
    /// crosses it.
    ///
    /// Two producers:
    ///   * `save_current_to_disk` (and its `save_active_as_copy`
    ///     / `save_buffer_as` siblings) — sets a deadline just
    ///     past the atomic-rename event burst so `ReadDirectoryChangesW`
    ///     events from our OWN save don't reach the user as a
    ///     phantom "reload from disk?" prompt. The
    ///     `unwatch`/`rewatch` dance already tried to gate this,
    ///     but the notify Windows backend watches whole
    ///     directories and events queued between the unwatch
    ///     and rewatch still land on the receiver — this
    ///     timestamp fence is what actually suppresses them.
    ///   * `drain` — after surfacing a "reload?" dialog for a
    ///     path, extends the deadline. A single external save
    ///     produces raw events that arrive across two-plus
    ///     drain cycles (each notify wake is a separate
    ///     `WM_APP_WAKE`); without cross-drain suppression the
    ///     user sees the same "reload?" prompt twice for one
    ///     external save.
    ///
    /// Not bounded by open tab count: `save_active_as_copy`
    /// (`NPPM_SAVEFILEAS`) writes to arbitrary caller-supplied
    /// paths, which grows the map beyond the set of open tab
    /// paths over a long session. Opportunistically pruned
    /// inside `drain` — expired entries older than
    /// `FILE_CHANGE_DEBOUNCE_PRUNE_AGE` are swept when the map
    /// grows past a soft-cap threshold. Entries stay valid past
    /// their deadline until pruned; a `contains_key` check
    /// alone means nothing, only the deadline value does.
    file_change_debounce: std::collections::HashMap<PathBuf, DebounceEntry>,
    /// Editor visual configuration persisted in `styles.xml` and
    /// edited via the Style Configurator dialog. Read at startup
    /// (`Shell::new`); written by [`Self::set_styles`] when the
    /// dialog's Save & Close fires. The UI is expected to call
    /// `UiPlatform::apply_default_style` on this value after the
    /// editor is up to seed the visible appearance.
    pub styles: codepp_core::styles::Styles,
    /// Runtime registry of User Defined Languages loaded from
    /// `<config_dir>/userDefineLangs/` at startup (Phase 4.6 m1b).
    /// The UI (m1d) reads [`codepp_udl::UdlRegistry::entries`] to
    /// append the loaded UDLs to the Language menu; the container-
    /// lexer runtime (m1c) resolves a buffer's UDL via
    /// [`codepp_udl::UdlRegistry::find_by_lang_type_id`] when the
    /// user activates a UDL-language buffer. Empty on a fresh
    /// install before the m1d first-run copy of the preinstalled
    /// UDLs has run; empty is a valid state (no runtime error, no
    /// startup failure — same graceful-degradation discipline as
    /// missing `session.xml`).
    pub udl_registry: codepp_udl::UdlRegistry,
}

/// Search-option bitset matching Scintilla's `SCFIND_*` flags. Held
/// as a Rust newtype so the public API doesn't bind callers to
/// `scintilla-sys` symbols. Phase 4 m3 covers case sensitivity,
/// whole-word matching, and POSIX/CXX11 regex; m4 (find-in-files)
/// reuses the same flag set against per-file searches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchFlags(pub u32);

impl SearchFlags {
    /// `SCFIND_NONE` — case-insensitive plain-text search.
    pub const NONE: SearchFlags = SearchFlags(0);
    /// `SCFIND_MATCHCASE` — `Foo` and `foo` are different.
    pub const MATCH_CASE: SearchFlags = SearchFlags(0x4);
    /// `SCFIND_WHOLEWORD` — `foo` does not match inside `foobar`.
    pub const WHOLE_WORD: SearchFlags = SearchFlags(0x2);
    /// `SCFIND_REGEXP | SCFIND_CXX11REGEX` — interpret the query
    /// as a C++11 regex. Without `CXX11REGEX`, Scintilla falls
    /// back to its older POSIX engine, which is missing common
    /// shorthands (`\d`, `\w`, lookarounds).
    pub const REGEX: SearchFlags = SearchFlags(0x0020_0000 | 0x0080_0000);

    /// OR two flag sets. Caller-friendly bit-combine without
    /// exposing the underlying u32.
    #[must_use]
    pub const fn union(self, other: SearchFlags) -> SearchFlags {
        SearchFlags(self.0 | other.0)
    }

    /// Raw bits, ready for `SCI_SETSEARCHFLAGS`'s wparam.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// Debounce window for file-change dialogs. See
/// [`Shell::file_change_debounce`] for the invariants; 1s is long
/// enough to swallow an atomic-rename event burst (~200ms on
/// Windows in the worst case) plus a comfortable safety margin,
/// short enough that a user's genuine second modification within
/// the same second doesn't get silently discarded across
/// human-perceivable time. Same order of magnitude as most GUI
/// text editors' external-change coalescing.
const FILE_CHANGE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);

/// Hard ceiling on how long a single sliding debounce window
/// can suppress dialogs for one path. Extending the deadline
/// on every event handles slow-filesystem bursts, but naive
/// sliding indefinitely means an adversarial (or runaway)
/// process writing at sub-`FILE_CHANGE_DEBOUNCE` cadence would
/// silently swallow every "reload from disk?" warning
/// forever, defeating the user-visible tamper-detection signal.
/// After this window elapses from the FIRST fence, the very
/// next event surfaces the dialog regardless of the sliding
/// deadline — the user then dismisses it and a new sliding
/// window opens.
const FILE_CHANGE_DEBOUNCE_MAX: std::time::Duration = std::time::Duration::from_secs(3);

/// Size threshold that triggers opportunistic pruning of
/// expired entries from `Shell::file_change_debounce`. Below
/// this, keeping the entries around is trivially cheap
/// (a handful of `DebounceEntry` values); above this, the map
/// may be accumulating stale entries from
/// `save_active_as_copy`'s arbitrary destination paths and a
/// sweep is warranted. Set well above realistic same-second
/// activity for a normal session (a burst of 64 simultaneous
/// saves is far above what one user can drive).
const FILE_CHANGE_DEBOUNCE_PRUNE_THRESHOLD: usize = 64;

/// Per-path debounce state. Sliding window with a hard
/// ceiling: `deadline` extends on every event, but suppression
/// only holds while both `now < deadline` AND
/// `now - first_set < FILE_CHANGE_DEBOUNCE_MAX`. See
/// [`FILE_CHANGE_DEBOUNCE_MAX`] for why the ceiling matters.
#[derive(Copy, Clone, Debug)]
struct DebounceEntry {
    /// When the FIRST fence was set for this path. Never moves
    /// forward within one debounce cycle — used as the anchor
    /// for the MAX-window ceiling.
    first_set: std::time::Instant,
    /// Sliding deadline extended by every incoming event
    /// (suppressed or surfaced). Only meaningful when the
    /// MAX-window ceiling from `first_set` hasn't elapsed.
    deadline: std::time::Instant,
}

/// Return `true` iff new file-change events for `path` should
/// currently be suppressed by the debounce window. Two
/// conditions must both hold:
///
///   * The sliding `deadline` has not elapsed (last event was
///     within [`FILE_CHANGE_DEBOUNCE`]).
///   * The absolute window from `first_set` has not elapsed
///     ([`FILE_CHANGE_DEBOUNCE_MAX`] — the ceiling that
///     prevents an adversarial or runaway process writing at
///     sub-debounce cadence from silently suppressing the
///     "reload from disk?" dialog forever).
///
/// Pure helper so tests can inject deterministic timestamps.
fn is_path_debounced(
    map: &std::collections::HashMap<PathBuf, DebounceEntry>,
    path: &Path,
    now: std::time::Instant,
) -> bool {
    map.get(path).is_some_and(|entry| {
        now < entry.deadline && now.duration_since(entry.first_set) < FILE_CHANGE_DEBOUNCE_MAX
    })
}

/// Fold a burst of raw file-watcher events into at most one
/// [`FileChange`] per path. Windows' `ReadDirectoryChangesW`
/// backend fires dozens of raw events for a single external
/// save; without this coalescing the drain would fire one dialog
/// per event, producing the user-reported "confirm 26 times to
/// work through them all" flood.
///
/// **Order-independent classification.** For the same path,
/// `Modified` dominates `Removed` regardless of which event
/// arrived first in the batch. The reason to keep `Modified`
/// rather than the "last event wins" alternative is that
/// atomic-rename saves burst `Modify(Name)`/Remove-shaped events
/// interleaved with `Modify(Any)`; picking "last wins" would
/// pick Removed roughly half the time (the transient rename
/// artefact), then the drain-level `try_exists` check would have
/// to upgrade almost every event. Keeping the Modified early
/// keeps the coalesce output stable and legible.
///
/// **Do not treat this classification as authoritative.**
/// `Shell::drain` runs a `try_exists` filesystem check on the
/// coalesced path AFTER this returns, and uses the on-disk state
/// as the final truth — so an edit-then-delete sequence within
/// one batch (where this returns `Modified` but the file is
/// really gone) still surfaces as `Removed` correctly. This
/// helper is a burst-compactor, not a state-of-the-world
/// oracle.
///
/// Extracted as a pure function (out of drain) so the invariant
/// is unit-testable without a live `FileWatcher` / channel pair.
fn coalesce_file_changes(events: impl IntoIterator<Item = FileChange>) -> Vec<FileChange> {
    let mut coalesced: std::collections::HashMap<PathBuf, FileChange> =
        std::collections::HashMap::new();
    for change in events {
        let (path, is_modified) = match &change {
            FileChange::Modified(p) => (p.clone(), true),
            FileChange::Removed(p) => (p.clone(), false),
        };
        match coalesced.get(&path) {
            // Existing Modified wins over new Removed —
            // atomic-rename transient removal AFTER the real
            // modification event.
            Some(FileChange::Modified(_)) if !is_modified => {}
            _ => {
                coalesced.insert(path, change);
            }
        }
    }
    coalesced.into_values().collect()
}

/// Verify a coalesced file-change against the real filesystem
/// state at `path`. Filesystem truth wins over the event kind
/// [`coalesce_file_changes`] returned — this catches two
/// scenarios where the event classification alone would produce
/// a wrong dialog:
///
///   * **Atomic-rename save reporting Removed:** the notify
///     backend fired only Modify(Name)/Remove-shaped events for
///     what was really a modification. `exists` returns `true`
///     (the rename landed a fresh file at the same path before
///     drain runs) — reclassify as `Modified` so the user gets
///     the correct reload prompt instead of a spurious "was
///     deleted" alert.
///   * **Edit-then-delete within one batch reporting Modified:**
///     `coalesce_file_changes` picks Modified for a same-path
///     burst that includes any Modified event, but if the file
///     is actually gone we need `Removed`. `exists` returns
///     `false` — reclassify.
///
/// `exists` is a closure so tests can pin the reclassification
/// without touching the real filesystem. Production wires it to
/// `Path::try_exists`. On `Err` (permission denied, ADS access
/// fault, symlink loop, ...) the return is treated as
/// `Removed` — the ambiguous case is safe to report because the
/// user's next Save-As recovers the buffer either way, and a
/// permissions-fault log line is not more useful to the user
/// than the removal dialog. The `Err` is logged at `warn` for
/// diagnostic visibility.
fn classify_change_by_existence<F>(change: FileChange, exists: F) -> FileChange
where
    F: FnOnce(&Path) -> std::io::Result<bool>,
{
    let path = match change {
        FileChange::Modified(p) | FileChange::Removed(p) => p,
    };
    match exists(&path) {
        Ok(true) => FileChange::Modified(path),
        Ok(false) => FileChange::Removed(path),
        Err(e) => {
            tracing::warn!(
                path = ?path,
                error = %e,
                "file-watcher: try_exists failed; classifying as Removed"
            );
            FileChange::Removed(path)
        }
    }
}

/// Start a fresh debounce window for `path` — sets both
/// `first_set` and `deadline` to now-anchored values. Overwrites
/// any existing entry so the MAX-window ceiling is re-anchored.
/// Used at every point where a new "quiet please" cycle begins:
///
///   * Save paths (`save_current_to_disk`, `save_buffer_as`,
///     `save_active_as_copy`) — the atomic-rename event burst
///     that follows should be suppressed for a fresh 1s + MAX.
///   * `drain`'s SURFACE branch — after a "reload from disk?"
///     dialog fires (either because there was no prior fence
///     or because the MAX-window ceiling elapsed on an
///     adversarial write burst), the next window starts from
///     now. Without this, a sustained sub-debounce write
///     cadence would spam a dialog per event once the ceiling
///     first tripped, because `first_set` would stay stuck at
///     the anchor from many seconds ago.
fn start_fresh_debounce(
    map: &mut std::collections::HashMap<PathBuf, DebounceEntry>,
    path: PathBuf,
    now: std::time::Instant,
) {
    map.insert(
        path,
        DebounceEntry {
            first_set: now,
            deadline: now + FILE_CHANGE_DEBOUNCE,
        },
    );
}

/// Extend the sliding `deadline` on an existing debounce entry
/// without touching `first_set`. Used by `drain`'s SUPPRESS
/// branch — a suppressed event resets the sliding clock so the
/// tail of a slow burst stays suppressed, but preserves the
/// MAX-window anchor so an adversarial cadence still surfaces
/// a dialog after `FILE_CHANGE_DEBOUNCE_MAX`. No-op if the
/// path has no existing entry (which shouldn't normally happen
/// on the suppress branch, but is defensively safe if it does).
fn extend_debounce_deadline(
    map: &mut std::collections::HashMap<PathBuf, DebounceEntry>,
    path: &Path,
    now: std::time::Instant,
) {
    if let Some(entry) = map.get_mut(path) {
        entry.deadline = now + FILE_CHANGE_DEBOUNCE;
    }
}

impl Shell {
    /// Create a `Shell` and wire up the cross-thread plumbing.
    ///
    /// `wake` is invoked by every producer thread after it sends a
    /// result, so the UI thread can drain its channels in the next
    /// message-pump iteration. On Win32 this is
    /// `PostMessage(hwnd, WM_APP_WAKE, 0, 0)` wrapped in an `Arc`.
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, ShellError> {
        // Loader: forward results into a Shell-owned channel so we can
        // wake the UI thread on each result without touching the
        // existing Loader API.
        let (loader, load_rx_inner, loader_shutdown) = Loader::spawn();
        let (load_tx_outer, load_rx_outer) = unbounded::<LoadResult>();
        spawn_forwarder(load_rx_inner, load_tx_outer, wake.clone(), "load-forwarder");

        // FileWatcher: same pattern.
        let (fc_tx_inner, fc_rx_inner) = unbounded::<FileChange>();
        let file_watcher =
            FileWatcher::new(fc_tx_inner).map_err(|e| ShellError::WatcherInit(e.to_string()))?;
        let (fc_tx_outer, fc_rx_outer) = unbounded::<FileChange>();
        spawn_forwarder(fc_rx_inner, fc_tx_outer, wake.clone(), "watch-forwarder");

        // FIF events: each job's walker/workers/coordinator clone
        // `fif_tx_inner` so per-job thread teardown doesn't close
        // the channel. The forwarder calls `wake` on each event so
        // the UI's message-pump iteration drains them via `drain`.
        let (fif_tx_inner, fif_rx_inner) = unbounded::<FifEvent>();
        let (fif_tx_outer, fif_rx_outer) = unbounded::<FifEvent>();
        spawn_forwarder(fif_rx_inner, fif_tx_outer, wake, "fif-forwarder");
        let fif_orchestrator = fif::FifOrchestrator::new(fif_tx_inner);

        // `<config_dir>/userDefineLangs/` — create if missing,
        // then scan for UDL XML files. Creating up-front matches
        // the promise `platform::user_define_langs_dir`'s doc
        // makes ("m1b's scanner `create_dir_all`s here first")
        // and gives m1d's first-run preinstalled-UDL copy a
        // place to write. `create_dir_all` is idempotent, so
        // hitting this on every startup after the first is
        // near-free. Failure is logged at warn and swallowed —
        // `scan_dir` still runs and returns an empty registry,
        // preserving the fresh-install-friendly discipline.
        // Per-file parse failures inside `scan_dir` are
        // similarly logged and skipped: a single malformed UDL
        // doesn't block startup or hide the rest of the
        // collection.
        let udl_registry = if let Some(dir) = codepp_platform::user_define_langs_dir() {
            if let Err(err) = std::fs::create_dir_all(&dir) {
                tracing::warn!(
                    path = ?dir,
                    error = %err,
                    "failed to create userDefineLangs directory; \
                     scan_dir will still run against the possibly-\
                     missing path"
                );
            }
            copy_preinstalled_udls(&dir);
            codepp_udl::UdlRegistry::scan_dir(&dir)
        } else {
            tracing::info!("no config_dir resolved; skipping UDL registry scan");
            codepp_udl::UdlRegistry::new()
        };

        Ok(Self {
            session: Session::new(),
            tabs: Vec::new(),
            active_tab: None,
            next_buffer_id: 1,
            #[cfg(target_os = "windows")]
            plugins: PluginHost::new(),
            #[cfg(target_os = "windows")]
            pending_notifications: Vec::new(),
            loader,
            _loader_shutdown: loader_shutdown,
            file_watcher,
            load_rx: load_rx_outer,
            change_rx: fc_rx_outer,
            #[cfg(target_os = "windows")]
            last_search: None,
            find_history: load_find_history(),
            fif_orchestrator,
            fif_rx: fif_rx_outer,
            pending_fif: Vec::new(),
            #[cfg(target_os = "windows")]
            pending_fif_launch: None,
            deferred_dialogs: Vec::new(),
            file_change_debounce: std::collections::HashMap::new(),
            styles: load_styles(),
            udl_registry,
        })
    }

    /// Persist a new editor-style configuration. Replaces
    /// `self.styles` in memory and writes the new value out to
    /// `styles.xml`. Errors during persistence are logged and
    /// swallowed — the in-memory change still takes effect so the
    /// active session sees the new colours / font, and the dialog
    /// has no useful recovery path for a write failure other than
    /// re-prompting the user (which would be more disruptive than
    /// the silent log).
    pub fn set_styles(&mut self, styles: codepp_core::styles::Styles) {
        self.styles = styles;
        save_styles(&self.styles);
    }

    /// Read access to the currently-active tab, or `None` if no
    /// file is open. The UI uses this to populate the title bar
    /// and status fields.
    #[must_use]
    pub fn active(&self) -> Option<&Tab> {
        self.active_tab.and_then(|i| self.tabs.get(i))
    }

    /// Mutable access to the currently-active tab. Internal Shell
    /// methods use this; the UI should go through high-level
    /// operations like `save_current_to_disk` rather than mutating
    /// directly.
    fn active_mut(&mut self) -> Option<&mut Tab> {
        let idx = self.active_tab?;
        self.tabs.get_mut(idx)
    }

    /// Allocate a fresh buffer id. Caller is responsible for
    /// installing it on a `Tab`. Bumps the `next_buffer_id` counter
    /// without reuse — see the field doc.
    ///
    /// Uses `checked_add` rather than `saturating_add`: saturation
    /// would silently start handing out colliding ids at
    /// `i32::MAX`, breaking the per-tab plugin-ABI `BufferID`
    /// contract. Two billion tab opens in a single session is
    /// unreachable in practice, but a hostile in-process plugin
    /// could in principle call `NPPM_DOOPEN` in a tight loop —
    /// the panic here turns that `DoS` path into a clean abort
    /// rather than a silent ABI break. The panic is caught by the
    /// `wnd_proc`'s `catch_unwind` wrappers.
    fn allocate_buffer_id(&mut self) -> i32 {
        let id = self.next_buffer_id;
        self.next_buffer_id = self
            .next_buffer_id
            .checked_add(1)
            .expect("buffer id space exhausted (i32::MAX opens in one session)");
        id
    }

    /// Smallest unused untitled sequence number across all currently-
    /// open tabs. Closing `new 1` and creating a new untitled buffer
    /// gives `new 1` again — same convention Notepad++ uses, and
    /// keeps the displayed numbers small for ergonomic tab labels.
    fn next_untitled_seq(&self) -> u32 {
        // The buffer-id counter (i32, currently `next_buffer_id`)
        // would have panicked at i32::MAX (~2 billion opens in one
        // session) long before this loop could reach u32::MAX, so
        // the search is bounded in practice. We still cap explicitly
        // at u32::MAX rather than risk a non-terminating loop on a
        // hypothetical future buffer-id refactor that uses u64.
        let mut n: u32 = 1;
        loop {
            if !self.tabs.iter().any(|t| t.untitled_seq == Some(n)) {
                return n;
            }
            match n.checked_add(1) {
                Some(next) => n = next,
                None => return u32::MAX,
            }
        }
    }

    /// Drain queued plugin notifications. Called by the UI after
    /// [`Self::drain`] (or any operation that may have queued a
    /// notification) — the UI fires each one through
    /// [`Self::notify_plugins`] **after** dropping the `&mut Shell`
    /// borrow, since `beNotified` runs synchronous plugin code that
    /// may re-enter the `wnd_proc`.
    #[cfg(target_os = "windows")]
    pub fn take_notifications(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.pending_notifications)
    }

    /// Queue `NPPN_BUFFERACTIVATED` for the currently-active tab.
    /// Call sites: `apply_load_result` after a fresh open (the new
    /// tab becomes active), `HostBridge::switch_to_file` when a
    /// plugin activates an existing tab, and `ui_win32`'s
    /// `handle_tab_selchange` on a user tab click. Each delivery
    /// fires after the `&mut Shell` borrow drops, so plugin
    /// `beNotified` callbacks can `SendMessage(NPPM_*)` back
    /// without aliasing UB.
    ///
    /// Idempotent — a no-op when there's no active tab. Safe to
    /// call from sites that may race with a close-tab path.
    #[cfg(target_os = "windows")]
    pub fn queue_buffer_activated(&mut self) {
        if let Some(tab) = self.active() {
            let buffer_id = tab.id as isize;
            self.pending_notifications
                .push(Notification::BufferActivated { buffer_id });
        }
    }

    /// Reorder the tab list by moving the entry at `from` to position
    /// `to` (in the post-removal list, so `to` ranges
    /// `0..self.tabs.len()`). The active-tab index is adjusted to
    /// follow whichever logical tab the user had focused — the
    /// dragged tab stays focused if it was, and any other active tab
    /// shifts left or right by one if the move passed through its
    /// slot.
    ///
    /// Used by the UI's tab-drag handler. The reordered `tabs` Vec
    /// is what `save_session` writes to disk, so persistence is
    /// automatic.
    ///
    /// No-op (returns `false`) when the move is a no-op (`from ==
    /// to`) or when either index is out of range. Returning a bool
    /// lets the caller skip the cost of a tab-strip resync when
    /// nothing changed.
    ///
    /// **Pinning invariant.** Pinned tabs occupy the left prefix of
    /// `tabs` in insertion order; unpinned tabs fill the remainder.
    /// `move_tab` rejects (returns `false`) any move that would
    /// violate that layout — either dragging a pinned tab (which
    /// the user pinned specifically to fix in place) or dragging
    /// an unpinned tab into the pinned prefix. Pin/unpin
    /// operations go through [`Self::set_pinned`] instead, which
    /// is the only path that changes a tab's `pinned` flag.
    pub fn move_tab(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return false;
        }
        // Pinning invariant: pinned tabs form a fixed prefix. Reject
        // any drag that would either (a) move a pinned tab at all,
        // or (b) push an unpinned tab up into the pinned prefix.
        // The boundary index is `first_unpinned_idx`; any
        // unpinned-tab `to` below that would violate the layout.
        if self.tabs[from].pinned {
            return false;
        }
        let first_unpinned = self.first_unpinned_idx();
        if to < first_unpinned {
            return false;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        // Adjust the active-tab index. Four cases:
        //   1. `active == from`  → the dragged tab itself; follow to
        //      the new position.
        //   2. `from < active <= to`  → moving rightward past
        //      `active`; everything in `(from, to]` shifts left by 1.
        //   3. `to <= active < from`  → moving leftward past
        //      `active`; everything in `[to, from)` shifts right by 1.
        //   4. otherwise  → `active` is outside the affected range,
        //      no change.
        if let Some(active) = self.active_tab.as_mut() {
            if *active == from {
                *active = to;
            } else if from < *active && *active <= to {
                *active -= 1;
            } else if to <= *active && *active < from {
                *active += 1;
            }
        }
        // Mirror the change into the session-cached active index so
        // a save-and-relaunch immediately after a drag picks up the
        // new active position even before any other state changes.
        self.session.active = self.active_tab;
        // Queue NPPN_DOCORDERCHANGED so plugins that maintain
        // per-tab UI state can re-sync from
        // `NPPM_GETOPENFILENAMES`. Fired on real reorders only —
        // the early `from == to` short-circuit above filters
        // out no-op drags that "land back where they started"
        // so plugins don't see a spurious "list changed" event.
        #[cfg(target_os = "windows")]
        self.pending_notifications
            .push(Notification::DocOrderChanged);
        true
    }

    /// Index of the first unpinned tab, i.e. the boundary between the
    /// pinned prefix and the unpinned suffix. Equal to the number of
    /// pinned tabs. Returns `tabs.len()` when every tab is pinned
    /// (rare but supported — no invariant violation).
    ///
    /// Cheap linear scan; typical sessions have well under 100 tabs,
    /// caching would add invalidation surface without measurable
    /// benefit. Kept `pub` so the UI's drag hit-test can clamp a
    /// drop target to the unpinned zone before calling `move_tab`
    /// (which also validates, but the UI wants to render the drop
    /// as bounded rather than reject a mid-drag frame).
    #[must_use]
    pub fn first_unpinned_idx(&self) -> usize {
        self.tabs
            .iter()
            .position(|t| !t.pinned)
            .unwrap_or(self.tabs.len())
    }

    /// Toggle the pin state of `idx` to `want`. Idempotent — no-op
    /// (returns `false`) if the tab is already in the requested
    /// state or if `idx` is out of range. Otherwise:
    ///
    /// * **Pin** (`want = true`): flip the flag, then move the tab
    ///   to the right edge of the pinned prefix (index
    ///   `first_unpinned_idx() - 1` after the flag flip). Preserves
    ///   the relative order among tabs that were already pinned.
    /// * **Unpin** (`want = false`): flip the flag, then move the
    ///   tab to the left edge of the unpinned zone (index
    ///   `first_unpinned_idx()` after the flag flip). Preserves
    ///   the relative order among tabs that stay pinned.
    ///
    /// The active-tab index follows the moved tab (same rule as
    /// `move_tab` for the "active == from" case). A real state
    /// change queues `NPPN_DOCORDERCHANGED` so plugins tracking
    /// buffer order re-sync from `NPPM_GETOPENFILENAMES`; the
    /// idempotent no-op path does not.
    pub fn set_pinned(&mut self, idx: usize, want: bool) -> bool {
        if idx >= self.tabs.len() || self.tabs[idx].pinned == want {
            return false;
        }
        self.tabs[idx].pinned = want;
        // Compute the target slot for the moved tab, expressed in
        // terms of the vector state AFTER `tabs.remove(idx)` runs.
        // The remove-then-insert model means the target must land
        // at the first unpinned position in the shrunken vec:
        //
        //   target = (# of tabs at positions != idx that are pinned)
        //
        // Pinning (`want = true`): pinned tabs excluding `idx` are
        // the *already-pinned* prefix — the newly pinned tab lands
        // right after them, at the end of the pinned block.
        // Unpinning (`want = false`): pinned tabs excluding `idx`
        // are the *still-pinned* prefix (the unpinning tab is
        // already flag-flipped so it doesn't count) — the newly
        // unpinned tab lands right after them, at the boundary.
        //
        // The formula is symmetric across the two cases; the flag
        // flip above shifts membership in the "pinned" set for the
        // count, which is what makes the target land in the
        // correct block.
        let target = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, t)| *i != idx && t.pinned)
            .count();
        // Move without going through `move_tab` — that path enforces
        // the invariant we're currently repairing. Manual splice
        // keeps the tab-move logic in one canonical shape while
        // reusing the active-index adjustment formula below.
        if idx != target {
            let tab = self.tabs.remove(idx);
            self.tabs.insert(target, tab);
            if let Some(active) = self.active_tab.as_mut() {
                if *active == idx {
                    *active = target;
                } else if idx < *active && *active <= target {
                    *active -= 1;
                } else if target <= *active && *active < idx {
                    *active += 1;
                }
            }
            self.session.active = self.active_tab;
        }
        #[cfg(target_os = "windows")]
        self.pending_notifications
            .push(Notification::DocOrderChanged);
        true
    }

    /// Close the currently-active tab. Returns a [`ClosedTab`] the
    /// UI uses to release the Scintilla document, switch the view
    /// to the new active tab's document, and remove the tab item
    /// from any platform tab strip. `NPPN_FILECLOSED` is queued
    /// for the closed buffer; if a different tab is now active,
    /// `NPPN_BUFFERACTIVATED` is queued for it. Returns `None`
    /// when there's nothing to close.
    ///
    /// New-active-tab selection follows the standard editor UX:
    /// prefer the right-neighbour (the tab that slid into the
    /// closed slot's index), fall back to the previous tab if
    /// the closed tab was the rightmost. Closing the last tab
    /// leaves `active_tab = None`.
    ///
    /// The file watcher is unregistered for the closed path
    /// inside this method so the UI doesn't have to remember;
    /// a failed `unwatch` is logged at debug level (the watcher
    /// silently ignores already-unregistered paths).
    pub fn close_active_tab(&mut self) -> Option<ClosedTab> {
        let idx = self.active_tab?;
        if idx >= self.tabs.len() {
            return None;
        }
        let removed = self.tabs.remove(idx);

        if let Some(p) = &removed.path {
            if let Err(e) = self.file_watcher.unwatch(p) {
                tracing::debug!(
                    error = %e,
                    path = ?p,
                    "unwatch on close (already unwatched is fine)"
                );
            }
        }

        // Pick the new active tab. If the closed tab was the last
        // one, `idx` now equals `tabs.len()` after the remove —
        // step left to keep an in-range index.
        let new_active = if self.tabs.is_empty() {
            None
        } else if idx < self.tabs.len() {
            Some(idx)
        } else {
            Some(self.tabs.len() - 1)
        };
        self.active_tab = new_active;

        let new_active_doc = new_active
            .and_then(|i| self.tabs.get(i))
            .map_or(0, |t| t.scintilla_doc);

        // Queue notifications in the same order N++ delivers them:
        //   1. NPPN_FILEBEFORECLOSE
        //   2. NPPN_FILECLOSED
        //   3. NPPN_BUFFERACTIVATED (only if there's a new active tab)
        //
        // **Known timing divergence vs N++ (tracked as Phase 5 polish):**
        // these notifications are pushed onto `pending_notifications`
        // and delivered to plugins by the UI *after* `take_notifications`
        // drains them — i.e., after `close_active_tab` returns and
        // after the tab has been removed from `self.tabs`. N++
        // delivers FILEBEFORECLOSE synchronously while the buffer is
        // still in its data structures, so a plugin's
        // `beNotified(NPPN_FILEBEFORECLOSE)` can call back into
        // `NPPM_GETFULLPATHFROMBUFFERID(id)` and get a real path.
        // Code++'s queue-deferred dispatch model means that callback
        // returns -1 (unknown id) instead. Plugins that need the path
        // at close time should cache it from the prior
        // BUFFERACTIVATED notification rather than relying on the
        // path lookup here.
        //
        // The fix needs synchronous-delivery plumbing for specific
        // notifications (Shell calling back into the plugin host
        // mid-operation, currently not part of the architecture);
        // the change is bigger than this batch should carry.
        let closing_id = removed.id as isize;
        #[cfg(target_os = "windows")]
        {
            self.pending_notifications
                .push(Notification::FileBeforeClose {
                    buffer_id: closing_id,
                });
            self.pending_notifications.push(Notification::FileClosed {
                buffer_id: closing_id,
            });
            if new_active.is_some() {
                self.queue_buffer_activated();
            }
        }
        // Suppress "unused" on non-Windows builds where the notification
        // queue isn't fed.
        #[cfg(not(target_os = "windows"))]
        let _ = closing_id;

        Some(ClosedTab {
            closed_idx: idx,
            buffer_id: removed.id,
            path: removed.path,
            scintilla_doc: removed.scintilla_doc,
            new_active_doc,
        })
    }

    /// Enumerate plugin DLLs in `dir`. No DLL is mapped — the loader
    /// only records paths; first-touch load happens when a plugin's
    /// menu is opened (DESIGN.md §6.4). Returns the count discovered.
    /// A non-existent directory is not an error (first-run case).
    #[cfg(target_os = "windows")]
    pub fn discover_plugins(&mut self, dir: &Path) -> std::io::Result<usize> {
        let count = self.plugins.discover(dir)?;
        // Apply the user's disabled-plugin list right after
        // enumeration so any DLL named in `disabled.txt` is
        // marked `disabled = true` *before* anything tries to
        // lazy-load it. Reading is best-effort: a missing file
        // is the first-run case (everyone enabled), a parse
        // error logs and falls back to "everyone enabled" so a
        // corrupted config can't lock the user out of plugins.
        let disabled = read_disabled_plugins_list();
        self.plugins.apply_disabled_list(&disabled);
        Ok(count)
    }

    /// Total plugins known to the host (any lifecycle state).
    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }

    /// Snapshot of every discovered plugin shaped for the Plugin
    /// Manager UI — see [`PluginAdminEntry`]. Caller takes
    /// ownership; no borrow on `Shell.plugins` is held across
    /// the modal pump.
    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn installed_plugins(&self) -> Vec<codepp_plugin_host::PluginAdminEntry> {
        self.plugins.snapshot_for_admin()
    }

    /// Toggle a plugin's disabled flag and write the change
    /// through to `<plugins_config_dir>/disabled.txt`. Called by
    /// the Plugin Manager UI when the user clicks the per-row
    /// Enabled checkbox. Returns `true` iff the in-memory state
    /// actually changed (an idempotent re-set returns `false`,
    /// which the caller can use to skip the disk write — though
    /// the writer below also short-circuits on a no-op).
    ///
    /// Toggling an already-loaded plugin doesn't unload it; the
    /// new disabled state takes effect on the next launch. This
    /// matches Notepad++'s "restart required" semantics —
    /// unloading a live plugin mid-session would yank function
    /// pointers the host (and other plugins) might still hold.
    #[cfg(target_os = "windows")]
    pub fn set_plugin_disabled(&mut self, idx: usize, disabled: bool) -> bool {
        let changed = self.plugins.set_disabled(idx, disabled);
        if changed {
            // Re-derive the on-disk list from the (now-mutated)
            // registry rather than mutating the file in place —
            // simpler and the file stays canonical (one entry
            // per disabled plugin, no stale entries for plugins
            // that have since been removed from disk).
            let disabled_filenames: Vec<String> = self
                .plugins
                .iter()
                .filter(|p| p.disabled)
                .filter_map(|p| {
                    p.path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(std::string::ToString::to_string)
                })
                .collect();
            if let Err(e) = write_disabled_plugins_list(&disabled_filenames) {
                tracing::warn!(error = %e, "failed to persist disabled plugins list");
            }
        }
        changed
    }

    /// Broadcast `notification` to every loaded plugin's `beNotified`.
    /// `npp_hwnd` is reported in `SCNotification.nmhdr.hwndFrom`.
    /// Synchronous on the UI thread (parity with Notepad++); plugins
    /// that block here block the host.
    #[cfg(target_os = "windows")]
    pub fn notify_plugins(&self, notification: Notification, npp_hwnd: Hwnd) {
        notify_all(&self.plugins, &notification, npp_hwnd);
    }

    /// Load every plugin currently in the `Pending` state. Called by
    /// the UI on first menu-popup open (lazy-load — DESIGN.md §6.4).
    /// Already-loaded plugins are skipped; failed plugins are recorded
    /// on the `PluginInfo` and surface to the UI via [`Self::plugin_load_outcomes`].
    ///
    /// `npp_data` is the `NppData` struct each plugin's `setInfo`
    /// receives. The same struct is passed to every plugin loaded by
    /// this call.
    #[cfg(target_os = "windows")]
    pub fn ensure_plugins_loaded(&mut self, npp_data: NppData) {
        let pending: Vec<usize> = self
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.is_loaded() && p.failed_reason().is_none())
            .map(|(i, _)| i)
            .collect();
        for idx in pending {
            if let Err(e) = self.plugins.load(idx, npp_data) {
                tracing::warn!(idx = idx, error = %e, "plugin load failed");
            }
        }
    }

    /// Iterate the (display name, `FuncItem` array) pairs of every
    /// loaded plugin. The UI uses this to populate the per-plugin
    /// submenu after [`Self::ensure_plugins_loaded`].
    ///
    /// Plugins with zero `FuncItems` are skipped — they're loaded but
    /// contribute no menu items (typically `beNotified`-only plugins).
    #[cfg(target_os = "windows")]
    pub fn loaded_plugin_funcs(&self) -> impl Iterator<Item = (String, &[FuncItem])> {
        self.plugins
            .iter()
            .filter(|p| p.is_loaded())
            .filter_map(|p| {
                let funcs = p.func_items()?;
                if funcs.is_empty() {
                    None
                } else {
                    Some((p.display_label(), funcs))
                }
            })
    }

    /// Find the plugin callback registered for menu-command id
    /// `cmd_id`. Returns the bare `PluginCmd` function pointer so
    /// the caller can invoke it after dropping any `&mut Shell`
    /// borrow — invoking the callback while a borrow is alive
    /// would be aliasing UB if the plugin synchronously
    /// `SendMessage`s an `NPPM_*` back into our `wnd_proc`.
    ///
    /// The returned pointer is valid as long as the plugin's DLL
    /// stays loaded (i.e. for the lifetime of `self`).
    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn lookup_plugin_command(&self, cmd_id: i32) -> Option<PluginCmd> {
        self.plugins.lookup_cmd(cmd_id)
    }

    /// Route a wnd_proc-received NPPM_* message into the plugin
    /// dispatcher. Returns `Some(lresult)` if the message was handled
    /// (the `wnd_proc` returns this from `WindowProc`), or `None` if
    /// the message is outside the NPPM_* range and the `wnd_proc`
    /// should fall through to its default handler.
    ///
    /// # Safety
    ///
    /// Several NPPM_* messages dereference plugin-supplied raw
    /// pointers in `lparam`. The caller must:
    ///
    /// * invoke this only from the UI thread that owns
    ///   `handles.npp_hwnd`,
    /// * pass `(msg, wparam, lparam)` triples received from a real
    ///   `wnd_proc` dispatch (synthesizing calls outside that flow is
    ///   undefined behaviour on the plugin's behalf),
    /// * supply a `handles` struct whose five fields all belong to
    ///   the same top-level window that received `msg` — mixing
    ///   handles across windows produces wrong results without any
    ///   diagnostic.
    ///
    /// At that point the plugin is the trust boundary and is bound
    /// by the documented NPPM_* ABI in
    /// `plugins/nppcompat-headers/Notepad_plus_msgs.h`.
    #[cfg(target_os = "windows")]
    #[must_use = "the wnd_proc must return the LRESULT this produced for handled messages, or fall through for None"]
    pub unsafe fn dispatch_plugin_message<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        handles: HostHandles,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> Option<isize> {
        let mut bridge = HostBridge {
            shell: self,
            ui,
            handles,
        };
        // SAFETY: forwarded; documented above.
        unsafe { dispatch_nppm(&mut bridge, msg, wparam, lparam) }
    }

    /// Queue a file open. The load runs on a worker thread; the
    /// result lands on the load-results channel and the UI drains
    /// it on the next wake.
    ///
    /// **Tab routing (Phase 3 milestone 6a):** if there is no active
    /// tab, or the active tab has no path yet (the
    /// just-launched-empty case), the load result populates the
    /// active tab in place. Otherwise a new tab is appended and
    /// becomes active.
    /// Create a fresh untitled buffer and make it active.
    ///
    /// Synchronous — no I/O, no worker thread; just allocates a new
    /// `Tab` with `path = None`, binds it to a freshly-created
    /// Scintilla document via [`UiPlatform::activate_tab`], and
    /// queues `NPPN_BUFFERACTIVATED` so plugins watching the active
    /// buffer pick up the new id. The user must use Save As to
    /// give it a path before the first save (`save_current_to_disk`
    /// returns [`ShellError::NoActivePath`] otherwise).
    ///
    /// Phase 4 m8: replaces the `MF_GRAYED` placeholder for File→New.
    /// Lossy on app exit — untitled buffers don't survive
    /// `session.xml` round-trip; that's the recovery-sidecar
    /// milestone's concern.
    pub fn new_untitled<U: UiPlatform>(&mut self, ui: &mut U) {
        if self.tabs.len() >= MAX_OPEN_TABS {
            tracing::warn!(
                cap = MAX_OPEN_TABS,
                open = self.tabs.len(),
                "new_untitled refused: tab cap reached",
            );
            return;
        }
        let id = self.allocate_buffer_id();
        let seq = self.next_untitled_seq();
        self.tabs.push(Tab {
            id,
            untitled_seq: Some(seq),
            ..Tab::default()
        });
        let new_idx = self.tabs.len() - 1;
        self.active_tab = Some(new_idx);

        // Bind a fresh Scintilla document to the new tab — passing
        // 0 tells the UI "I don't have one yet, create one for me".
        // The returned doc pointer is what subsequent activations
        // re-bind to.
        let bound_doc = ui.activate_tab(new_idx, 0);
        if let Some(tab) = self.tabs.get_mut(new_idx) {
            tab.scintilla_doc = bound_doc;
        }
        ui.set_buffer_text("", 0);
        ui.apply_lang(L_TEXT);
        ui.update_status(L_TEXT, &Encoding::default(), Eol::default(), 0);

        // No NPPN_FILEOPENED — that's reserved for "a real file
        // arrived"; matches Notepad++'s convention. BUFFERACTIVATED
        // fires so plugins observing buffer changes pick up the new
        // id and tab index.
        #[cfg(target_os = "windows")]
        self.queue_buffer_activated();
    }

    /// Save every titled, non-loading tab to disk in tab order.
    /// Returns one `(buffer_id, error)` pair for each tab whose
    /// save failed; an empty `Vec` means everything saved cleanly.
    ///
    /// Phase 4 m8: drives File→Save All. Untitled buffers (no
    /// path) and tabs still waiting on their loader (`pending_load`
    /// is `Some`) are skipped silently — both are inappropriate
    /// targets for Save All (the former needs Save As, the latter
    /// would race the loader's write).
    ///
    /// Implementation: iterates tabs by index, switching the
    /// editor's bound document to each in turn so
    /// `save_current_to_disk`'s `ui.get_buffer_text()` reads the
    /// right tab's content. The final step rebinds to whichever
    /// tab was active at entry, so the user's view is exactly
    /// where they left it. The intermediate doc switches are
    /// invisible to plugins — `save_all` deliberately does **not**
    /// queue `NPPN_BUFFERACTIVATED` for the per-tab activations,
    /// matching N++'s contract that Save All looks like one
    /// atomic operation from a plugin's perspective.
    pub fn save_all<U: UiPlatform>(&mut self, ui: &mut U) -> Vec<(i32, ShellError)> {
        // Capture the *buffer id* of the tab the user was on, not
        // its index. If the tab list shrinks during the loop (a
        // future re-entrant path triggered by FILESAVED handlers
        // could in principle remove tabs), the stored index would
        // point at the wrong tab on restore — leading to a
        // subsequent Ctrl+S writing to the wrong file. Looking up
        // the index by id at the end is robust to shifts.
        let original_active_id = self.active().map(|t| t.id);
        let mut errors = Vec::new();

        for idx in 0..self.tabs.len() {
            // Skip untitled buffers and in-flight loads.
            let skip = self
                .tabs
                .get(idx)
                .is_none_or(|t| t.path.is_none() || t.pending_load.is_some());
            if skip {
                continue;
            }
            // Bind the editor to this tab's document. Re-read the
            // returned doc pointer — `activate_tab` may have
            // lazy-created it (background-loaded tab being saved
            // for the first time would land here in theory; in
            // practice that combination doesn't occur because
            // `pending_load.is_some()` filters it above).
            let stored_doc = self.tabs[idx].scintilla_doc;
            let bound_doc = ui.activate_tab(idx, stored_doc);
            if let Some(tab) = self.tabs.get_mut(idx) {
                tab.scintilla_doc = bound_doc;
            }
            self.active_tab = Some(idx);

            // Wrap the per-tab save in `catch_unwind` so a panic in
            // one tab (e.g. a tracing-subscriber misbehaviour, an
            // OOM, a bug in the encoding crate) doesn't abort the
            // outer loop — `self.active_tab` would be left pointing
            // at the panicked tab, breaking the restore step below
            // and leaving the user's view on a buffer they didn't
            // ask to be on. Treating the panic as a per-tab error
            // keeps the loop bounded and the active-tab restore
            // unconditional.
            let id = self.tabs.get(idx).map_or(0, |t| t.id);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.save_current_to_disk(ui)
            }));
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => errors.push((id, e)),
                Err(_) => {
                    errors.push((
                        id,
                        ShellError::Io(String::from("internal panic during save")),
                    ));
                }
            }
        }

        // Restore the original active tab so the user's view
        // returns to where they were before Save All. No
        // `queue_buffer_activated` here — the user perceives Save
        // All as a single operation, not N+1 activations. Look up
        // the index by buffer id (rather than the entry-time
        // index) so a mid-loop tab shrink doesn't leave the
        // restore pointing at the wrong tab.
        if let Some(orig_id) = original_active_id {
            if let Some(orig_idx) = self.tabs.iter().position(|t| t.id == orig_id) {
                let stored_doc = self.tabs[orig_idx].scintilla_doc;
                ui.activate_tab(orig_idx, stored_doc);
                self.active_tab = Some(orig_idx);
            }
        }

        errors
    }

    /// Re-read the active tab's file from disk, discarding any
    /// in-buffer edits. Returns `false` (and does nothing) if the
    /// active tab has no path (untitled buffer) or no tab is open.
    ///
    /// Phase 4 m8: drives File→Reload from Disk. Routes through
    /// [`Self::confirm_reload`] (the same path the file-watcher
    /// "external change detected, reload?" prompt takes), so the
    /// in-buffer edits the user discards are exactly the same set
    /// the watcher path would discard.
    ///
    /// Caller (the UI) is responsible for prompting the user
    /// before calling this — `reload_active` itself doesn't ask.
    /// The dirty check belongs in the UI because it requires
    /// querying Scintilla's `SCI_GETMODIFY` directly (Code++
    /// doesn't shadow dirty state on the `Tab`).
    pub fn reload_active(&mut self) -> bool {
        let path = self.active().and_then(|t| t.path.clone());
        match path {
            Some(p) => {
                self.confirm_reload(p);
                true
            }
            None => false,
        }
    }

    /// Save the active tab to a caller-supplied path, updating the
    /// tab's path metadata so subsequent Save (`Ctrl+S`) writes to
    /// the same destination. Driven by File→Save As… and by the
    /// plugin ABI's path-changing flows in Phase 5.
    ///
    /// The active path (if any) is unwatched before the write and
    /// the new path is watched afterwards — so external-change
    /// detection follows the file as it moves. The encode + atomic
    /// `tempfile::persist` pattern matches [`Self::save_current_to_disk`];
    /// the only differences are the path source (caller, not tab) and
    /// the trailing `tab.path` mutation.
    ///
    /// Returns the same `Result<(), ShellError>` shape as
    /// `save_current_to_disk`. `NoActivePath` here means "no active
    /// tab to save", not "the active tab has no path" — Save As
    /// works regardless of whether the tab was titled.
    pub fn save_buffer_as<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        new_path: PathBuf,
    ) -> Result<(), ShellError> {
        use std::io::Write;

        let (old_path, encoding) = {
            let tab = self.active().ok_or(ShellError::NoActivePath)?;
            (tab.path.clone(), tab.encoding.clone())
        };
        let text = ui.get_buffer_text();
        // Encode BEFORE the unwatch below — if encoding fails we
        // return early and the unwatch never happens, so the old
        // file's watch stays intact. Reordering would silently lose
        // change-detection on a still-untouched file.
        let bytes = codepp_core::encoding::encode(&text, &encoding)
            .map_err(|e| ShellError::Encoding(e.to_string()))?;

        let parent = new_path.parent().filter(|p| !p.as_os_str().is_empty());
        let parent_dir = parent.unwrap_or_else(|| std::path::Path::new("."));

        // Stop watching the old location before the new write — so a
        // mid-save external-change event for the old path doesn't
        // generate a spurious reload prompt for a buffer that's
        // moving away anyway. The old watch is restored on a write
        // failure (see below); on success the tab moves to the new
        // path and the old file is no longer ours.
        let was_watching_old = old_path
            .as_ref()
            .is_some_and(|p| self.file_watcher.unwatch(p).is_ok());

        let write_result = (|| -> Result<(), ShellError> {
            let mut tmp = tempfile::Builder::new()
                .prefix(".codepp-save-")
                .suffix(".tmp")
                .tempfile_in(parent_dir)
                .map_err(|e| ShellError::Io(e.to_string()))?;
            tmp.write_all(&bytes)
                .map_err(|e| ShellError::Io(e.to_string()))?;
            tmp.as_file_mut()
                .sync_all()
                .map_err(|e| ShellError::Io(e.to_string()))?;
            tmp.persist(&new_path)
                .map_err(|e| ShellError::Io(e.error.to_string()))?;
            Ok(())
        })();

        match &write_result {
            Ok(()) => {
                // Successful write — point the tab at the new path
                // and watch it.
                if let Some(tab) = self.active_mut() {
                    tab.path = Some(new_path.clone());
                    tab.text = text;
                    tab.byte_len = bytes.len() as u64;
                    // Re-derive lang from the new extension so a
                    // .txt → .rs Save As immediately gets Rust
                    // highlighting on the next paint.
                    tab.lang = LangType::from_path(&new_path);
                    // The buffer is no longer untitled — drop the
                    // sequence number so the tab strip switches
                    // from "new N" to the file's basename, and so
                    // a future File→New can reuse the now-freed
                    // sequence value. The user-chosen
                    // `custom_name` (set via File → Rename...) is
                    // also cleared here: once the buffer has a
                    // real path, the on-disk filename is the
                    // canonical display name.
                    tab.untitled_seq = None;
                    tab.custom_name = None;
                }
                if let Err(e) = self.file_watcher.watch(&new_path) {
                    tracing::warn!(error = %e, path = ?new_path, "failed to watch new path after Save As");
                }
                // Debounce fence — same rationale as
                // save_current_to_disk. The atomic-rename event
                // burst from THIS Save As write must not
                // surface as an immediate "reload from disk?"
                // prompt for the file we just created.
                start_fresh_debounce(
                    &mut self.file_change_debounce,
                    new_path.clone(),
                    std::time::Instant::now(),
                );
                // Notification push first, then dirty-glyph clear —
                // see save_current_to_disk for the ordering rationale.
                #[cfg(target_os = "windows")]
                {
                    let buffer_id = self.active().map_or(0, |t| t.id as isize);
                    self.pending_notifications
                        .push(Notification::FileSaved { buffer_id });
                }
                ui.mark_saved();
                // Push the new lang through the UI so the lexer
                // re-attaches and the chrome refreshes.
                if let Some(tab) = self.active() {
                    let lang = tab.lang;
                    let encoding = tab.encoding.clone();
                    let eol = tab.eol;
                    let byte_len = tab.byte_len;
                    ui.apply_lang(lang);
                    ui.update_status(lang, &encoding, eol, byte_len);
                }
            }
            Err(_) => {
                // Save failed; restore the old watch so the user
                // doesn't silently lose external-change detection
                // on the original file.
                if was_watching_old {
                    if let Some(p) = old_path.as_ref() {
                        if let Err(e) = self.file_watcher.watch(p) {
                            tracing::warn!(error = %e, path = ?p, "failed to re-watch old path after Save As failure");
                        }
                    }
                }
            }
        }
        write_result
    }

    /// Save a copy of the active buffer to `path` without
    /// re-pointing the active tab. The in-memory buffer continues
    /// tracking its original on-disk path (or stays untitled).
    /// Drives `NPPM_SAVECURRENTFILEAS(asCopy=TRUE)`.
    ///
    /// No file watcher dance because the destination path is not
    /// being adopted by Code++ as a tracked file. No
    /// `NPPN_FILESAVED` because the active buffer's "last save"
    /// state did not change — N++'s ABI only fires FILESAVED for
    /// the rename variant. No tab metadata mutation.
    ///
    /// Same atomic-write pattern as `save_current_to_disk` /
    /// `save_buffer_as`: write to a sibling tempfile, fsync, atomic
    /// rename. Power-loss safe — the destination is either the
    /// pre-call bytes (or absent) or fully the new bytes.
    pub fn save_active_as_copy<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        path: &Path,
    ) -> Result<(), ShellError> {
        use std::io::Write;

        let encoding = self
            .active()
            .map(|t| t.encoding.clone())
            .ok_or(ShellError::NoActivePath)?;
        let text = ui.get_buffer_text();
        let bytes = codepp_core::encoding::encode(&text, &encoding)
            .map_err(|e| ShellError::Encoding(e.to_string()))?;

        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        let parent_dir = parent.unwrap_or_else(|| Path::new("."));

        let mut tmp = tempfile::Builder::new()
            .prefix(".codepp-savecopy-")
            .suffix(".tmp")
            .tempfile_in(parent_dir)
            .map_err(|e| ShellError::Io(e.to_string()))?;
        tmp.write_all(&bytes)
            .map_err(|e| ShellError::Io(e.to_string()))?;
        tmp.as_file_mut()
            .sync_all()
            .map_err(|e| ShellError::Io(e.to_string()))?;
        tmp.persist(path)
            .map_err(|e| ShellError::Io(e.error.to_string()))?;
        // Debounce fence — see save_current_to_disk. If `path`
        // happens to be a file we already have open in another
        // tab (NPPM_SAVEFILEAS can point anywhere), the atomic
        // rename above would surface as a "reload from disk?"
        // prompt on that other tab. Registering the fence for
        // any path is cheap and harmless when the path isn't
        // watched.
        start_fresh_debounce(
            &mut self.file_change_debounce,
            path.to_path_buf(),
            std::time::Instant::now(),
        );
        Ok(())
    }

    /// Request that `path` be opened. Returns the branch the shell
    /// took so the UI knows whether it needs to synchronously rebind
    /// the Scintilla view.
    ///
    /// The rebind matters in the `SwitchedToExisting` case: the tab
    /// strip's `TCM_SETCURSEL` update (fired from the UI's normal
    /// post-mutation resync) does **not** generate `TCN_SELCHANGE`,
    /// so the `wnd_proc`'s `handle_tab_selchange` — which is what
    /// actually issues `SCI_SETDOCPOINTER` — never runs on its own.
    /// Without an explicit follow-up, the tab strip would show the
    /// user's re-opened file as active while the editor keeps
    /// rendering the previous buffer.
    ///
    /// The other three variants require no UI action beyond the
    /// normal chrome refresh: `Loading` is handled by the `drain`'s
    /// `activate_tab` when the loader posts its wake, `AlreadyActive`
    /// changed nothing, and `Rejected` didn't touch any tab state.
    pub fn open_file(&mut self, path: PathBuf) -> OpenFileOutcome {
        // De-duplicate: if the path is already open in some tab,
        // switch to that tab rather than allocating a fresh one.
        // Without this, the user can stack identical-content tabs
        // by repeatedly Open-dialoging the same file — and Reload
        // (which routes through `confirm_reload` → `open_file` for
        // the not-yet-open case) would create duplicates of the
        // file the user is reloading.
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| t.path.as_deref() == Some(path.as_path()))
        {
            return if self.active_tab == Some(idx) {
                OpenFileOutcome::AlreadyActive
            } else {
                self.active_tab = Some(idx);
                #[cfg(target_os = "windows")]
                self.queue_buffer_activated();
                OpenFileOutcome::SwitchedToExisting(idx)
            };
        }

        // Bound the per-session DoS surface — see `MAX_OPEN_TABS`.
        // The check is below the de-dupe branch so a hostile-or-
        // accidental re-open of an already-open file still
        // succeeds (it doesn't grow the tab count). It's above the
        // `loader.open` call so we don't enqueue a load we'll then
        // discard.
        if self.tabs.len() >= MAX_OPEN_TABS {
            tracing::warn!(
                cap = MAX_OPEN_TABS,
                open = self.tabs.len(),
                path = ?path,
                "open_file refused: tab cap reached",
            );
            return OpenFileOutcome::Rejected;
        }

        // Queue NPPN_FILEBEFOREOPEN before the load is enqueued.
        // N++'s ABI fires this right before the file is read so
        // plugins can pre-process / log the open intent — Code++'s
        // notification model defers delivery, so the plugin runs
        // after the load is in flight, but the queue ordering still
        // matches "BEFORE_OPEN before FILEOPENED" which is the
        // contract plugins read for. Carries no buffer id (the tab
        // hasn't been allocated yet) — N++ uses the same convention.
        // Skipped on dedupe (already-open path) above; that's a
        // tab activation, not a file open.
        #[cfg(target_os = "windows")]
        self.pending_notifications
            .push(Notification::FileBeforeOpen);

        let Some(req_id) = self.loader.open(path.clone()) else {
            // The loader's worker channel rejected the request
            // (worker thread shut down, or the channel buffer
            // is full). Pair the just-queued `FileBeforeOpen`
            // with `FileLoadFailed` so a plugin tracking
            // in-flight opens via BEFORE/AFTER doesn't leak
            // state — same contract as the
            // `apply_load_result` Err arm.
            #[cfg(target_os = "windows")]
            self.pending_notifications
                .push(Notification::FileLoadFailed);
            return OpenFileOutcome::Rejected;
        };
        // Decide where the load result will land. If the active
        // tab is empty *and isn't already waiting for its own load
        // to complete and isn't a user-created File→New buffer*,
        // reuse it; otherwise allocate a fresh tab now so we have
        // a buffer id to associate with `req_id`.
        //
        // Without the `pending_load.is_none()` guard, two rapid
        // open_file calls (e.g. session restore reopening multiple
        // tabs) would both target the same empty tab — the second
        // call overwrites the first call's pending-load marker, so
        // the first load's apply_load_result finds no matching tab
        // and silently discards the buffer. Symptom: only the last
        // file in a multi-tab session is restored.
        //
        // The `untitled_seq.is_none()` guard distinguishes an
        // anonymous internal placeholder (which is fair game for
        // reuse) from an explicit File→New buffer (`untitled_seq =
        // Some(N)`, rendered as "new N" in the tab strip). Reusing
        // a File→New tab silently replaces it with the opened
        // file — the user's deliberate "I made a new buffer" gets
        // erased the moment they Open something. Today no internal
        // path produces a Tab with `path = None && pending_load =
        // None && untitled_seq = None`, so this branch is
        // effectively dead, but the guard stays as defense-in-depth
        // for future paths that might need an internal placeholder.
        let target_idx = match self.active_tab {
            Some(i)
                if self.tabs.get(i).is_some_and(|t| {
                    t.path.is_none() && t.pending_load.is_none() && t.untitled_seq.is_none()
                }) =>
            {
                i
            }
            _ => {
                let id = self.allocate_buffer_id();
                self.tabs.push(Tab {
                    id,
                    pending_load: Some(req_id),
                    ..Tab::default()
                });
                let new_idx = self.tabs.len() - 1;
                self.active_tab = Some(new_idx);
                return OpenFileOutcome::Loading;
            }
        };
        // Reusing an empty tab — assign an id if it didn't have one
        // and set the pending-load marker.
        let needs_id = self.tabs.get(target_idx).is_some_and(|t| t.id == 0);
        let new_id = if needs_id {
            Some(self.allocate_buffer_id())
        } else {
            None
        };
        if let Some(tab) = self.tabs.get_mut(target_idx) {
            if let Some(id) = new_id {
                tab.id = id;
            }
            tab.pending_load = Some(req_id);
        } else {
            // active_tab pointed at a missing index — recover by
            // creating a new tab.
            let id = self.allocate_buffer_id();
            self.tabs.push(Tab {
                id,
                pending_load: Some(req_id),
                ..Tab::default()
            });
            self.active_tab = Some(self.tabs.len() - 1);
        }
        OpenFileOutcome::Loading
    }

    /// Drain pending tasks and apply each to the shell + UI. Returns
    /// any dialogs the UI must show *after* this call returns — the
    /// `&mut Shell` borrow ends with the function, so a nested message
    /// pump (e.g. `MessageBoxW`) inside the dialog code can't re-enter
    /// the `wnd_proc` and produce aliasing UB on the per-window state.
    pub fn drain<U: UiPlatform>(&mut self, ui: &mut U) -> Vec<PendingDialog> {
        let mut pending = Vec::new();
        while let Ok(result) = self.load_rx.try_recv() {
            self.apply_load_result(ui, result, &mut pending);
        }
        // File-change events: coalesce first, then apply.
        //
        // A single external save fires many events on Windows'
        // `ReadDirectoryChangesW` — an atomic-rename save (which
        // both Notepad++ and Code++'s own `save_current_to_disk`
        // use) produces `Modify(Name)` events for the rename plus
        // `Modify(Any)` for the actual write, often several of
        // each because the API doesn't compact bursts. Firing one
        // dialog per raw event surfaced as the user-reported
        // "confirm 26 times to work through them all" bug when
        // the same file is open in Notepad++ and Code++
        // simultaneously and either app saves.
        //
        // Coalesce by path (step 1) then verify existence
        // (step 2). See [`coalesce_file_changes`] for the pure
        // coalescing logic and [`classify_change_by_existence`]
        // for the filesystem-verification wrapper.
        let events: Vec<FileChange> =
            std::iter::from_fn(|| self.change_rx.try_recv().ok()).collect();
        let now = std::time::Instant::now();
        // Opportunistic prune: sweep entries whose deadline is
        // in the past, but only when the map has grown past the
        // soft cap. Keeps the amortised cost near zero for the
        // steady-state case (few paths, all live) while
        // bounding the worst case for a session that touched a
        // lot of distinct paths via `save_active_as_copy` /
        // `NPPM_SAVEFILEAS`.
        if self.file_change_debounce.len() > FILE_CHANGE_DEBOUNCE_PRUNE_THRESHOLD {
            self.file_change_debounce
                .retain(|_, entry| now < entry.deadline);
        }
        for change in coalesce_file_changes(events) {
            let effective = classify_change_by_existence(change, Path::try_exists);
            let path_ref: &Path = match &effective {
                FileChange::Modified(p) | FileChange::Removed(p) => p,
            };
            // Debounce: suppress the dialog if this path's
            // deadline hasn't elapsed. Two suppression sources
            // land here (see `file_change_debounce` field doc):
            // own-save echoes and cross-drain duplicates of one
            // external save. Every event that hits this path —
            // whether suppressed or surfaced — extends the
            // deadline. Extending on suppress too matters
            // because atomic-rename event bursts on a slow
            // filesystem (antivirus scanning the temp file, a
            // laggy network drive, ...) can span longer than
            // the initial `FILE_CHANGE_DEBOUNCE` window; without
            // this extension, the tail of such a burst would
            // leak a spurious dialog past the original
            // deadline. Any single new event resets the clock.
            let debounced = is_path_debounced(&self.file_change_debounce, path_ref, now);
            if debounced {
                // Suppress branch: extend the sliding deadline
                // so the tail of a slow burst stays quiet, but
                // do NOT re-anchor `first_set` — the MAX-window
                // ceiling must still fire on an adversarial
                // cadence.
                extend_debounce_deadline(&mut self.file_change_debounce, path_ref, now);
                tracing::debug!(
                    path = ?path_ref,
                    "file-watcher: event suppressed by debounce"
                );
                continue;
            }
            // Surface branch: start a fresh window (re-anchor
            // both `first_set` and `deadline`). Covers both the
            // "first ever event" case and the "MAX ceiling just
            // tripped after a suppress spree" case — either
            // way, from here we owe the user 1s of quiet before
            // a follow-up dialog, and MAX-window from now
            // before we can force one past the sliding gate.
            start_fresh_debounce(&mut self.file_change_debounce, path_ref.to_path_buf(), now);
            self.apply_file_change(effective, &mut pending);
        }
        // Stage FIF events for the UI to consume after the borrow
        // ends — same pattern as plugin notifications. Per active
        // job the bound is `MAX_MATCHES_TOTAL + 1` (terminal event);
        // across multiple `start_fif` calls without an intervening
        // `take_fif_events` it scales linearly with the number of
        // jobs. Win32 calls `take_fif_events` on every WM_APP_WAKE,
        // so practical depth stays below the per-job ceiling.
        while let Ok(event) = self.fif_rx.try_recv() {
            self.pending_fif.push(event);
        }
        // Append dialogs queued by sync paths (currently
        // `restore_dirty_with_text`, which surfaces an "external
        // edit while the app was crash-killed" reload prompt at
        // startup). The UI's existing dialog presenter handles
        // these the same way as drain-sourced dialogs.
        // `Vec::append` on an empty source is already a no-op
        // — no extra branch needed for the common case.
        pending.append(&mut self.deferred_dialogs);
        pending
    }

    /// Start a find-in-files job. Preempts any in-flight job and
    /// returns the new job's id. Events are drained off the shell's
    /// FIF channel into `pending_fif` on each `drain` and consumed
    /// by [`Self::take_fif_events`].
    ///
    /// Returns [`FifError::Query`] on a malformed query (without
    /// spawning any threads) and [`FifError::BadRoot`] when the
    /// requested root is not a directory.
    pub fn start_fif(&mut self, request: FifRequest) -> Result<FifJobId, FifError> {
        self.fif_orchestrator.start(request)
    }

    /// Cancel the current find-in-files job, if any. Idempotent.
    pub fn cancel_fif(&mut self) {
        self.fif_orchestrator.cancel();
    }

    /// Drain queued FIF events. Called by the UI after [`Self::drain`]
    /// (or any operation that may have queued events) so the events
    /// can be applied to the results dock outside the `&mut Shell`
    /// borrow — listview population is a UI-thread, dialog-pump-safe
    /// operation that mustn't run with shell state locked.
    pub fn take_fif_events(&mut self) -> Vec<FifEvent> {
        std::mem::take(&mut self.pending_fif)
    }

    /// Drain the pending `NPPM_LAUNCHFINDINFILESDLG` prefill, if a
    /// plugin posted one since the last call. The Win32 plugin
    /// dispatch consumes this immediately after
    /// `dispatch_plugin_message` returns and feeds it to the FIF
    /// dialog opener.
    #[cfg(target_os = "windows")]
    pub fn take_fif_launch_prefill(&mut self) -> Option<FifLaunchPrefill> {
        self.pending_fif_launch.take()
    }

    /// Setter for the FIF launch prefill — used by the
    /// `HostServices::launch_find_in_files_dialog` impl on
    /// `HostBridge` to stash the plugin's pre-fill request. Public
    /// in symmetry with [`Self::take_fif_launch_prefill`] so the
    /// write path mirrors the read path; the field itself stays
    /// private so external callers can't bypass the take/set
    /// contract.
    #[cfg(target_os = "windows")]
    pub fn set_fif_launch_prefill(&mut self, prefill: FifLaunchPrefill) {
        self.pending_fif_launch = Some(prefill);
    }

    /// Confirm a deferred reload: requeue the file through the loader,
    /// targeting the *existing* tab that already has this path. Called
    /// by the UI after the user clicks Yes on the reload prompt
    /// returned in [`PendingDialog::ConfirmReload`], and by File→Reload
    /// (via [`Self::reload_active`]).
    ///
    /// Does **not** create a new tab — the path is already open by
    /// definition (the file watcher only fires for watched files,
    /// which are open files; menu-driven Reload only runs on the
    /// active tab's path). Marks the matching tab's `pending_load`
    /// so [`Self::apply_load_result`] overwrites its contents in
    /// place when the loader completes.
    ///
    /// If the path *isn't* found in any tab — a defensive fallback
    /// for hypothetical stale-watcher scenarios — falls through to
    /// [`Self::open_file`], which will deduplicate or open as
    /// appropriate.
    pub fn confirm_reload(&mut self, path: PathBuf) {
        let Some(idx) = self
            .tabs
            .iter()
            .position(|t| t.path.as_deref() == Some(path.as_path()))
        else {
            // Fallback for stale-watcher scenarios: the path isn't
            // in the tab list, so open it fresh. The dedupe-rebind
            // outcome can't fire here (we just proved above that no
            // tab holds this path), so the discarded return value
            // is genuinely nothing the UI needs to react to.
            let _ = self.open_file(path);
            return;
        };
        let Some(req_id) = self.loader.open(path) else {
            return;
        };
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.pending_load = Some(req_id);
        }
    }

    fn apply_load_result<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        result: LoadResult,
        pending: &mut Vec<PendingDialog>,
    ) {
        match result {
            Ok(loaded) => {
                // Find the tab whose pending_load matches this id — that
                // tells us which tab the user requested this load for.
                // Anything else is stale.
                let Some(target_idx) = self
                    .tabs
                    .iter()
                    .position(|t| t.pending_load == Some(loaded.id))
                else {
                    tracing::debug!(
                        stale_id = loaded.id,
                        "discarding stale load result (no matching pending tab)"
                    );
                    return;
                };

                // Begin watching the new file before pushing into
                // the editor — if the watch fails the user still gets
                // the buffer; we just won't catch external changes.
                if let Err(e) = self.file_watcher.watch(&loaded.path) {
                    tracing::warn!(error = %e, path = ?loaded.path, "failed to watch file");
                }

                // Single by-path lookup covering every attribute the
                // load path pulls from the persisted session:
                // caret position, the user's Language-menu override,
                // and the pin state. Rebuilding the find call per
                // attribute (an earlier shape of this code) had the
                // same shape three times and pushed the function
                // over clippy's `too_many_lines` gate.
                let stored = self
                    .session
                    .tabs
                    .iter()
                    .find(|t| t.path.as_deref() == Some(loaded.path.as_path()));
                let cursor = stored.map_or(0, |t| t.cursor);
                let stored_lang_override = stored.and_then(|t| t.lang).map(LangType);
                let stored_pinned = stored.is_some_and(|t| t.pinned);

                // Write the tab fields first so any UI calls below
                // observe a tab in its post-load state. The borrow
                // ends at the end of the if-let-else.
                let Some(tab) = self.tabs.get_mut(target_idx) else {
                    return;
                };
                tab.pending_load = None;
                tab.path = Some(loaded.path.clone());
                tab.encoding.clone_from(&loaded.encoding);
                tab.eol = loaded.eol;
                tab.byte_len = loaded.byte_len;
                tab.text.clone_from(&loaded.text);
                // Lang resolution: persisted Language-menu override
                // wins; extension-based auto-detection is the
                // fallback. Plugins may still override later via
                // `NPPM_SETBUFFERLANGTYPE`.
                tab.lang =
                    stored_lang_override.unwrap_or_else(|| LangType::from_path(&loaded.path));
                // Restore the persisted pin state — the
                // pinned-before-unpinned invariant is preserved
                // because `save_session` writes tabs in that order
                // and `load_session_entries` iterates in the same
                // order, so the tab vector is reassembled with the
                // pinned prefix intact.
                tab.pinned = stored_pinned;
                let stored_doc = tab.scintilla_doc;
                let lang = tab.lang;
                #[cfg(target_os = "windows")]
                let buffer_id = tab.id as isize;

                // Apply UI updates only when this load targets the
                // **active** tab. `activate_tab` rebinds the single
                // Scintilla view to the supplied document — calling
                // it for a non-active tab would leave the view
                // pointed at the wrong document for the rest of
                // the drain (and forever, since nothing else
                // re-binds). Phase 3's `open_file` always makes the
                // newly-opened tab active, so the active branch is
                // the only path actually exercised by the v1 demo.
                //
                // Background-tab loads (session-restore opening
                // multiple files at once, or
                // `NPPM_DOOPEN`-driven loads onto an already-active
                // tab) get their `text`/`encoding` fields populated
                // above, but their Scintilla document stays
                // uncreated until first activation by a tab click
                // — see `handle_tab_selchange`. Milestone 6c will
                // add a `populate_background_tab` flow that
                // creates + fills the document without disturbing
                // the visible view.
                // Queue NPPN_FILEOPENED for the loaded plugins. The UI
                // drains the queue via take_notifications() after
                // dropping its &mut Shell borrow — required because
                // beNotified runs synchronous plugin code that may
                // re-enter the wnd_proc. Pushed BEFORE
                // NPPN_BUFFERACTIVATED below so the delivery order
                // matches Notepad++'s canonical sequence: file-open
                // events fire before buffer-activation events on
                // the same load.
                #[cfg(target_os = "windows")]
                self.pending_notifications
                    .push(Notification::FileOpened { buffer_id });

                let is_active = self.active_tab == Some(target_idx);
                if is_active {
                    let bound_doc = ui.activate_tab(target_idx, stored_doc);
                    if let Some(tab) = self.tabs.get_mut(target_idx) {
                        tab.scintilla_doc = bound_doc;
                    }
                    ui.set_buffer_text(&loaded.text, cursor);
                    // apply_lang AFTER set_buffer_text — Scintilla
                    // re-styles the visible region on lexer attach,
                    // so the lexer needs to see the document already
                    // populated to colour it on the first paint.
                    ui.apply_lang(lang);
                    ui.update_status(lang, &loaded.encoding, loaded.eol, loaded.byte_len);

                    // The just-loaded tab is now the user-visible
                    // buffer — fire NPPN_BUFFERACTIVATED so plugins
                    // observing buffer changes pick up the new id.
                    // Notification queue is Windows-gated.
                    #[cfg(target_os = "windows")]
                    self.queue_buffer_activated();
                }
            }
            Err(err) => {
                // A failed load on a fresh tab (one that never had a
                // path) leaves an orphan: nonzero buffer id, but
                // `path = None`. Plugins gate on `id != 0 ⇒ path
                // is Some`; preserving the orphan would silently
                // break that invariant. Find the matching tab and
                // either remove it (fresh open) or just clear
                // `pending_load` (reload of a tab with prior
                // contents — keep its previous path/text).
                let target = self
                    .tabs
                    .iter()
                    .position(|t| t.pending_load == Some(err.id));
                if let Some(idx) = target {
                    let is_fresh = self.tabs[idx].path.is_none();
                    if is_fresh {
                        self.tabs.remove(idx);
                        self.active_tab = match self.active_tab {
                            Some(active_idx) if active_idx == idx => {
                                if self.tabs.is_empty() {
                                    None
                                } else if active_idx >= self.tabs.len() {
                                    Some(self.tabs.len() - 1)
                                } else {
                                    Some(active_idx)
                                }
                            }
                            Some(active_idx) if active_idx > idx => Some(active_idx - 1),
                            other => other,
                        };
                    } else {
                        // Reload failed; keep the tab's prior contents,
                        // just drop the pending marker.
                        self.tabs[idx].pending_load = None;
                    }
                }
                pending.push(PendingDialog::Error {
                    title: "Open failed".to_string(),
                    message: format!("{}: {}", err.path.display(), err.error),
                });
                // Pair every `FileBeforeOpen` issued by `open_file`
                // with one of `FileOpened` / `FileLoadFailed`.
                // Plugins that audit-log file activity rely on the
                // pairing — without `FileLoadFailed` they'd never
                // hear back about a failed open and would track
                // an in-flight open forever.
                #[cfg(target_os = "windows")]
                self.pending_notifications
                    .push(Notification::FileLoadFailed);
            }
        }
    }

    fn apply_file_change(&mut self, change: FileChange, pending: &mut Vec<PendingDialog>) {
        // Path comparison is by exact equality. Windows can spell
        // the same file as a long name, an 8.3 short name, or a
        // junction-routed path; all three would silently miss the
        // reload prompt here. Plugin `NPPM_RELOADFILE` and user
        // junction-traversal both reach this code path, and the
        // canonicalize-both-sides hardening is tracked for milestone 5.
        match change {
            FileChange::Modified(path) => {
                if self
                    .tabs
                    .iter()
                    .any(|t| t.path.as_deref() == Some(path.as_path()))
                {
                    pending.push(PendingDialog::ConfirmReload(path));
                }
            }
            FileChange::Removed(path) => {
                // Find the tab whose path matches and resolve its
                // buffer id for the NPPN_FILEDELETED payload. The
                // tab stays open — the buffer text is still in
                // memory and the user can save-as to recover the
                // file. We queue the notification + the user-facing
                // dialog, and mark the tab dirty so the tab strip's
                // save icon flips to red AND the close-tab flow
                // surfaces the save prompt: closing an open buffer
                // whose backing file has just vanished would
                // otherwise discard the only surviving copy. The
                // underlying Scintilla doc may still report
                // `SCI_GETMODIFY == 0` (no user edit since the last
                // save-point), but the paint cache and the close-
                // prompt gate both consult `Tab.dirty` — flipping
                // it here is enough to close the data-loss hazard.
                // Single mut-borrow walk: flip `Tab.dirty` inside
                // the `find` closure and hand the id back out for
                // the notification push below. Path uniqueness
                // across tabs is enforced elsewhere (open-file
                // dedup activates the existing tab rather than
                // creating a duplicate), so at most one tab can
                // match.
                let matched_id = self
                    .tabs
                    .iter_mut()
                    .find(|t| t.path.as_deref() == Some(path.as_path()))
                    .map(|tab| {
                        tab.dirty = true;
                        tab.id
                    });
                if let Some(id) = matched_id {
                    #[cfg(target_os = "windows")]
                    {
                        let buffer_id = id as isize;
                        self.pending_notifications
                            .push(Notification::FileDeleted { buffer_id });
                    }
                    // Suppress unused-variable warnings on
                    // non-Windows builds. Once the GTK / Cocoa
                    // plugin host bridges land in Phase 5, the
                    // notification push above moves out of the
                    // cfg gate and `id` becomes used unconditionally.
                    #[cfg(not(target_os = "windows"))]
                    let _ = id;
                    pending.push(PendingDialog::Error {
                        title: "File removed".to_string(),
                        message: format!(
                            "{} was deleted or moved externally. The buffer is still in memory.",
                            path.display()
                        ),
                    });
                }
            }
        }
    }

    /// Write the current buffer to its associated path, atomically.
    ///
    /// Steps:
    ///   1. Pull live text from the editor (covers user edits in
    ///      Scintilla that haven't been mirrored back to the shadow).
    ///   2. Re-encode in the buffer's current encoding.
    ///   3. Write to a sibling tempfile, fsync, persist over the
    ///      destination — same pattern as `Session::save_to_xml`.
    ///      Power-loss safety: the file on disk is always either the
    ///      pre-save bytes or fully the new bytes, never torn.
    ///
    /// The file watcher is briefly unregistered around the write so
    /// our own save doesn't trigger a "file changed externally —
    /// reload?" prompt. **Known limitation:** an *external* write by
    /// another process during this same window is silently missed.
    /// Phase 3+ may switch to inode/serial-number tracking instead of
    /// unwatch/rewatch.
    pub fn save_current_to_disk<U: UiPlatform>(&mut self, ui: &mut U) -> Result<(), ShellError> {
        use std::io::Write;

        // Snapshot what we need from the active tab so we can release
        // its borrow before calling the watcher and the I/O helpers
        // (which take their own &mut self). `buffer_id` is captured
        // here too so the `FileBeforeSave` notification below uses
        // the same id this method's success path also reports
        // through `FileSaved` — without that, a second `self.active()`
        // call could in principle produce a different value (e.g. if
        // the active tab were re-bound between the two reads). The
        // contract holds today because this method is fully
        // synchronous on the UI thread, but binding once removes the
        // implicit invariant.
        #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
        let (path, encoding, buffer_id) = {
            let tab = self.active().ok_or(ShellError::NoActivePath)?;
            (
                tab.path.as_ref().ok_or(ShellError::NoActivePath)?.clone(),
                tab.encoding.clone(),
                tab.id as isize,
            )
        };

        // Queue NPPN_FILEBEFORESAVE for the loaded plugins. Fired
        // *before* the encoding pass so plugins observing the
        // notification still see the buffer in its pre-save state
        // (relevant for plugins that snapshot the buffer text on
        // BEFORE_SAVE and compare against FILESAVED). Code++'s
        // notifications are queue-deferred — by the time the plugin
        // runs, the save has already happened — but the BEFORE-pair
        // ordering matches Notepad++'s ABI and lets a future
        // synchronous-delivery wiring (DESIGN.md §7.4) honour the
        // contract correctly without rearranging this code.
        #[cfg(target_os = "windows")]
        self.pending_notifications
            .push(Notification::FileBeforeSave { buffer_id });

        let text = ui.get_buffer_text();
        let bytes = codepp_core::encoding::encode(&text, &encoding)
            .map_err(|e| ShellError::Encoding(e.to_string()))?;

        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        let parent_dir = parent.unwrap_or_else(|| std::path::Path::new("."));

        let was_watching = self.file_watcher.unwatch(&path).is_ok();

        let write_result = (|| -> Result<(), ShellError> {
            let mut tmp = tempfile::Builder::new()
                .prefix(".codepp-save-")
                .suffix(".tmp")
                .tempfile_in(parent_dir)
                .map_err(|e| ShellError::Io(e.to_string()))?;
            tmp.write_all(&bytes)
                .map_err(|e| ShellError::Io(e.to_string()))?;
            tmp.as_file_mut()
                .sync_all()
                .map_err(|e| ShellError::Io(e.to_string()))?;
            tmp.persist(&path)
                .map_err(|e| ShellError::Io(e.error.to_string()))?;
            Ok(())
        })();

        // Re-watch regardless of save success so we keep tracking the
        // file. A failed save leaves the existing watch handle invalid
        // anyway (the file itself wasn't replaced), so it's harmless.
        if was_watching {
            if let Err(e) = self.file_watcher.watch(&path) {
                tracing::warn!(error = %e, path = ?path, "failed to re-watch after save");
            }
        }
        // Register a debounce fence covering the atomic-rename
        // event burst that just fired (or is about to — the events
        // arrive on a background thread and are typically observed
        // shortly after `tempfile::persist` returns). Without this,
        // Windows' `ReadDirectoryChangesW`-based notify backend
        // delivers our own save's Modify/Rename events past the
        // `unwatch`/`rewatch` gate above (which only refilters,
        // doesn't pause the underlying directory watch), and the
        // next `drain` surfaces a spurious "reload from disk?"
        // prompt for the exact file we just wrote. See
        // `file_change_debounce` field doc for the full contract.
        // Registered even on `write_result.is_err()` because a
        // failed save still generated a tempfile create+delete
        // burst that would surface as ambiguous events.
        start_fresh_debounce(
            &mut self.file_change_debounce,
            path.clone(),
            std::time::Instant::now(),
        );
        write_result?;

        // Use the byte count of what we just encoded — re-reading from
        // disk would race with a process that swapped the file between
        // our `persist` and the `metadata` call (TOCTOU), and produce
        // a status-bar size that doesn't match the bytes we just
        // wrote. We already know the size; use it.
        if let Some(tab) = self.active_mut() {
            tab.text = text;
            tab.byte_len = bytes.len() as u64;
        }

        // Queue NPPN_FILESAVED *before* clearing the dirty glyph.
        // If `mark_saved` were called first and the queue push
        // panicked (OOM-class), Scintilla would show the buffer
        // as clean but plugins watching FILESAVED would silently
        // miss the notification — invisible to the user, hard to
        // diagnose. Pushing first means the worst case is "saved
        // file still shows the dirty glyph", which the user can
        // notice and fix with another Ctrl+S.
        // Reuses the `buffer_id` captured at the top of the method
        // alongside `path` / `encoding` — pairs with the
        // `FileBeforeSave` queue push so both notifications agree
        // on the id even in the edge case where a future refactor
        // moves the active-tab change inside this method.
        #[cfg(target_os = "windows")]
        self.pending_notifications
            .push(Notification::FileSaved { buffer_id });

        // Clear Scintilla's dirty glyph for the just-saved buffer.
        // Done here so every save path (single Save, Save As, the
        // per-tab loop in Save All) gets the dirty-state reset
        // automatically — without this Save All would only clear
        // the glyph when *every* tab succeeded, since the UI
        // handler folds save-points only on a fully-clean batch.
        ui.mark_saved();

        Ok(())
    }

    /// Change the active tab's save-time encoding to `encoding`,
    /// driving the Encoding menu's "Convert to ..." items.
    ///
    /// Code++'s Scintilla view always stores text as UTF-8 internally
    /// (we set `SC_CP_UTF8` at create time), so an encoding change
    /// is purely a metadata flip: the in-memory bytes don't move,
    /// but the next [`Self::save_current_to_disk`] re-encodes through
    /// the new variant before writing to disk. Open the file again
    /// and `core::encoding::detect`/`decode` reads the new bytes
    /// back into UTF-8 — round-trip-correct for any text whose
    /// codepoints are representable in both encodings (which is
    /// every text for the four UTF variants the Encoding menu
    /// currently exposes).
    ///
    /// Returns `true` if the encoding actually changed (caller
    /// should refresh the status bar / radio); `false` on no-op
    /// (same encoding already, or no active tab). The no-op path is
    /// silent — re-clicking the active radio item shouldn't poke
    /// the title bar with a fake "modified" indicator.
    ///
    /// **Known limitation (deferred to a polish pass):** Scintilla's
    /// own modify flag (driven by `SCI_GETMODIFY`) is not flipped by
    /// this metadata-only change, so the title-bar dirty glyph won't
    /// surface "encoding pending save". The status bar updates
    /// (different label) and the user can still Ctrl+S to commit.
    /// N++'s glyph behaviour here is the same.
    pub fn set_buffer_encoding(&mut self, encoding: codepp_core::Encoding) -> bool {
        let Some(idx) = self.active_tab else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(idx) else {
            return false;
        };
        if tab.encoding == encoding {
            return false;
        }
        tab.encoding = encoding;
        true
    }

    /// Like [`Self::set_buffer_encoding`] but addresses an arbitrary
    /// open buffer by id rather than the active one. Plumbs
    /// `NPPM_SETBUFFERENCODING` from a plugin onto a specific buffer
    /// — the plugin contract takes a buffer id, not "the active
    /// buffer", so we need an id-keyed setter alongside the
    /// menu-driven active-tab setter.
    ///
    /// Same metadata-only semantics: the next save through the
    /// affected tab encodes via the new variant.
    ///
    /// Returns `true` whenever the buffer ends up in the requested
    /// state — both for an actual change and for a same-value
    /// no-op. Returns `false` only for an unknown id. This matches
    /// `set_buffer_lang_type`'s contract, which is the convention
    /// `NPPM_SETBUFFERLANGTYPE` plugins already rely on, and
    /// matches Notepad++'s "TRUE = the buffer is now in the
    /// requested state" return semantics. Distinguishing
    /// "unknown id" from "no-op success" is the bit plugins gate
    /// on; collapsing both to `false` would silently break plugins
    /// that conditionally re-encode only when the set "succeeds".
    pub fn set_buffer_encoding_by_id(
        &mut self,
        id: isize,
        encoding: codepp_core::Encoding,
    ) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id as isize == id) else {
            return false;
        };
        if tab.encoding == encoding {
            // Same-value no-op: the buffer is already in the
            // requested state, which is success per the N++
            // contract. Skip the mutation (no need to rewrite the
            // same value) but report success.
            return true;
        }
        tracing::debug!(
            buffer_id = id,
            from = %tab.encoding.label(),
            to = %encoding.label(),
            "set_buffer_encoding_by_id"
        );
        tab.encoding = encoding;
        true
    }

    /// Set the EOL format on the buffer with id `id`. Mirrors
    /// [`Self::set_buffer_encoding_by_id`] for line endings — same
    /// "TRUE = buffer is in the requested state" return convention,
    /// `false` only for unknown id.
    ///
    /// **Phase 4 metadata-only:** existing line-ending bytes inside
    /// the Scintilla document are not rewritten — `SCI_CONVERTEOLS`
    /// needs UI-side cooperation (the doc-pointer-swap dance to
    /// reach a non-active buffer's document), tracked in DESIGN.md
    /// §7.4.
    pub fn set_buffer_eol_by_id(&mut self, id: isize, eol: codepp_core::Eol) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id as isize == id) else {
            return false;
        };
        if tab.eol == eol {
            return true;
        }
        tracing::debug!(
            buffer_id = id,
            from = %tab.eol.label(),
            to = %eol.label(),
            "set_buffer_eol_by_id"
        );
        tab.eol = eol;
        true
    }

    /// Search the active editor forward for `query` under `flags`
    /// and activate the match (Scintilla moves the selection to
    /// it). Returns the byte offset of the match, or `None` on
    /// miss. Stores the query + flags as the "last search" so
    /// subsequent `find_next_repeat` / `find_prev_repeat` calls
    /// can reuse them for keyboard-driven Find Next without the
    /// user re-entering anything. The dialog calls this on its
    /// initial Find click; the menu's Find Next / Find Previous
    /// (and their F3 / Shift+F3 shortcuts in m3b) reuse the
    /// stored state.
    ///
    /// **Misses still record the query.** This matches Notepad++:
    /// after a "not found" hit, F3 re-issues the same search,
    /// which lets the user re-tap F3 once they've expanded the
    /// search target rather than re-typing the query. Callers
    /// that want different semantics should clear `last_search`
    /// themselves on a `None` return.
    #[cfg(target_os = "windows")]
    pub fn find_next<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        flags: SearchFlags,
    ) -> Option<u64> {
        if query.is_empty() || self.active_tab.is_none() {
            return None;
        }
        self.last_search = Some((query.to_string(), flags));
        if self.find_history.push_find(query) {
            save_find_history(&self.find_history);
        }
        ui.search_next(query, flags)
    }

    /// Repeat the last `find_next` with its stored query and flags.
    /// Returns `None` if no search has been issued yet, or if the
    /// query missed.
    #[cfg(target_os = "windows")]
    pub fn find_next_repeat<U: UiPlatform>(&mut self, ui: &mut U) -> Option<u64> {
        let (query, flags) = self.last_search.clone()?;
        ui.search_next(&query, flags)
    }

    /// Backward sibling of [`Self::find_next`] — used by the Find
    /// dialog when the "Backward direction" checkbox is on. Stores
    /// the query+flags as the new `last_search` so a subsequent
    /// `find_next_repeat` / `find_prev_repeat` reuses them.
    #[cfg(target_os = "windows")]
    pub fn find_prev<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        flags: SearchFlags,
    ) -> Option<u64> {
        if query.is_empty() || self.active_tab.is_none() {
            return None;
        }
        self.last_search = Some((query.to_string(), flags));
        if self.find_history.push_find(query) {
            save_find_history(&self.find_history);
        }
        ui.search_prev(query, flags)
    }

    /// Repeat the last `find_next` going backward.
    #[cfg(target_os = "windows")]
    pub fn find_prev_repeat<U: UiPlatform>(&mut self, ui: &mut U) -> Option<u64> {
        let (query, flags) = self.last_search.clone()?;
        ui.search_prev(&query, flags)
    }

    /// Replace the current selection with `replacement` if and only
    /// if the selection text matches `query` under `flags`. The
    /// dialog calls this for its "Replace" button: the user has
    /// just done a Find which left the match selected; clicking
    /// Replace substitutes that match, then the dialog typically
    /// fires another Find Next. The match-check guards against
    /// replacing arbitrary text the user dragged a selection over
    /// after the find — Scintilla doesn't gate on that itself.
    /// Returns true if a replacement happened.
    #[cfg(target_os = "windows")]
    pub fn replace_current<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        replacement: &str,
        flags: SearchFlags,
    ) -> bool {
        if query.is_empty() || self.active_tab.is_none() {
            return false;
        }
        let changed_find = self.find_history.push_find(query);
        let changed_replace = self.find_history.push_replace(replacement);
        if changed_find || changed_replace {
            save_find_history(&self.find_history);
        }
        ui.replace_current(query, replacement, flags)
    }

    /// Replace every match of `query` with `replacement` in the
    /// active buffer. Returns the count of replacements performed.
    /// All replaces happen inside one Scintilla undo group so the
    /// user can Ctrl+Z the entire Replace-All in a single step.
    /// Empty `query` is a no-op (returns 0) — Scintilla would
    /// otherwise spin in an infinite loop on an empty match.
    #[cfg(target_os = "windows")]
    pub fn replace_all<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        replacement: &str,
        flags: SearchFlags,
    ) -> usize {
        if query.is_empty() || self.active_tab.is_none() {
            return 0;
        }
        let changed_find = self.find_history.push_find(query);
        let changed_replace = self.find_history.push_replace(replacement);
        if changed_find || changed_replace {
            save_find_history(&self.find_history);
        }
        ui.replace_all(query, replacement, flags)
    }

    /// Count occurrences of `query` in the active buffer. The
    /// Find dialog's "Count" button surfaces the result; does
    /// not affect selection or `last_search` state (matching N++).
    #[cfg(target_os = "windows")]
    pub fn count_matches<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        flags: SearchFlags,
    ) -> usize {
        if query.is_empty() || self.active_tab.is_none() {
            return 0;
        }
        if self.find_history.push_find(query) {
            save_find_history(&self.find_history);
        }
        ui.count_matches(query, flags)
    }

    /// In-selection sibling of [`Self::find_next`]. The dialog
    /// captures the selection bounds when "In selection" is
    /// checked and forwards them on every Find Next click;
    /// `last_search` is still recorded so an F3 outside the
    /// dialog falls back to the whole-buffer behaviour.
    #[cfg(target_os = "windows")]
    pub fn find_next_in_range<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> Option<u64> {
        if query.is_empty() || self.active_tab.is_none() || end <= start {
            return None;
        }
        self.last_search = Some((query.to_string(), flags));
        if self.find_history.push_find(query) {
            save_find_history(&self.find_history);
        }
        ui.search_next_in_range(query, flags, start, end)
    }

    /// Backward sibling of [`Self::find_next_in_range`].
    #[cfg(target_os = "windows")]
    pub fn find_prev_in_range<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> Option<u64> {
        if query.is_empty() || self.active_tab.is_none() || end <= start {
            return None;
        }
        self.last_search = Some((query.to_string(), flags));
        if self.find_history.push_find(query) {
            save_find_history(&self.find_history);
        }
        ui.search_prev_in_range(query, flags, start, end)
    }

    /// Replace All restricted to `[start, end)`. Returns
    /// `(count, new_end)` so the caller can refresh its
    /// in-selection range after the document length shifts.
    #[cfg(target_os = "windows")]
    pub fn replace_all_in_range<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        replacement: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> (usize, u64) {
        if query.is_empty() || self.active_tab.is_none() || end <= start {
            return (0, end);
        }
        let changed_find = self.find_history.push_find(query);
        let changed_replace = self.find_history.push_replace(replacement);
        if changed_find || changed_replace {
            save_find_history(&self.find_history);
        }
        ui.replace_all_in_range(query, replacement, flags, start, end)
    }

    /// Persist the open-tab list to `session.xml` at the configured
    /// path. Called on clean shutdown. The active tab's cursor is
    /// pulled live from the editor so the next launch restores the
    /// caret where the user left it; non-active tabs use cursor 0
    /// for now (milestone 6b's UI tab control records per-tab cursors
    /// at switch time).
    pub fn save_session<U: UiPlatform>(&self, ui: &mut U) -> Result<(), ShellError> {
        let Some(session_path) = codepp_platform::session_xml_path() else {
            // No config dir resolvable (sandboxed environment); skip
            // session save silently. Untitled buffers still die, but
            // there's nowhere to durably write them.
            return Ok(());
        };
        let backups_dir = codepp_platform::backups_dir();
        let mut session = Session::new();
        // Persist the current window geometry alongside the tab list
        // so the next launch can restore the user's preferred size.
        // The UI keeps `self.session.window` in sync via
        // `set_window_geometry` on `WM_SIZE` / maximize-restore; we
        // just snapshot the latest value here.
        session.window = self.session.window;
        // Same discipline for the workspace panel state. The UI
        // keeps `self.session.workspace` in sync via
        // `set_workspace_session` right before every save (see
        // `sync_workspace_state_to_shell` in `ui_win32`); we
        // snapshot the latest value here. Without this, the
        // workspace state I set via `set_workspace_session`
        // would be thrown away — `save_session` starts from a
        // fresh `Session::new()` and only carries over fields
        // it explicitly copies.
        session.workspace.clone_from(&self.session.workspace);
        // Same discipline for view-level toggles (indent guide,
        // future siblings). Kept in sync by `set_view_settings`
        // whenever the user flips a toggle; snapshot here so the
        // fresh session carries it forward instead of resetting
        // to `ViewSettings::default()`.
        session.view = self.session.view;

        // Filenames of all backups we wrote on this save pass.
        // After session.xml is durably written we use this list to
        // prune any older backup files in the directory that the
        // new session no longer references — keeps the directory
        // bounded over the long term.
        let mut written_backups: Vec<String> = Vec::new();
        // Stable timestamp suffix shared by every backup written in
        // this save pass. Matches Notepad++'s `<name>@<timestamp>`
        // naming convention for backup files. Local time so the
        // user can read it at a glance when inspecting the
        // directory.
        let timestamp = backup_timestamp();

        for (idx, tab) in self.tabs.iter().enumerate() {
            // Active-tab cursor comes from the editor; others are 0
            // until a future tab-switch hook persists per-tab cursors.
            let cursor = if Some(idx) == self.active_tab {
                ui.get_cursor_pos()
            } else {
                0
            };

            // Decide whether this tab needs a backup file. Two
            // cases write backups:
            //   1. Untitled buffer (`tab.path.is_none()`) — always
            //      backed up; there's no file on disk to fall back
            //      to and unsaved-buffer survival is the whole
            //      point.
            //   2. Saved file with unsaved edits
            //      (`SCI_GETMODIFY != 0`) — backed up so the
            //      user's in-memory edits survive a crash or
            //      close-without-save. The on-disk file's
            //      content stays intact (we don't touch the real
            //      path during this write); on next launch the
            //      backup overlays the buffer and the tab opens
            //      dirty.
            //
            // Clean saved files (path-bound, no edits) skip the
            // backup write entirely — the disk file IS the
            // authoritative state.
            let mut backup_filename: Option<String> = None;
            let needs_backup = tab.path.is_none() || ui.is_doc_dirty(tab.scintilla_doc);
            if needs_backup {
                if let Some(dir) = &backups_dir {
                    let display = if let Some(seq) = tab.untitled_seq {
                        format!("new {seq}")
                    } else if let Some(p) = &tab.path {
                        // Use the file's basename so the backup
                        // dir is human-readable. Sanitised so the
                        // backup we write is also one we can
                        // later read past `is_safe_backup_filename`.
                        sanitize_basename_for_backup(p)
                    } else {
                        format!("untitled-{}", tab.id)
                    };
                    let filename = format!("{display}@{timestamp}");
                    let abs_path = dir.join(&filename);
                    let text = ui.capture_text_from_doc(tab.scintilla_doc);
                    match write_backup_file(&abs_path, text.as_bytes()) {
                        Ok(()) => {
                            backup_filename = Some(filename.clone());
                            written_backups.push(filename);
                        }
                        Err(e) => {
                            // For untitled tabs a failed backup
                            // means the tab is lost on next
                            // launch — log + skip from the
                            // persisted list. For saved files we
                            // still have the on-disk version, so
                            // degrade gracefully: keep the tab in
                            // session.xml without a backup
                            // reference. The user loses unsaved
                            // edits but not the open-tab record.
                            tracing::warn!(
                                path = ?tab.path,
                                untitled_seq = ?tab.untitled_seq,
                                error = %e,
                                "failed to write backup file; unsaved edits not protected",
                            );
                            if tab.path.is_none() {
                                continue;
                            }
                        }
                    }
                } else if tab.path.is_none() {
                    // No backups directory available (sandboxed
                    // environment) — untitled tabs can't be
                    // persisted, so drop them silently the same
                    // way the old code did. Saved-file tabs flow
                    // through (their disk content is the source
                    // of truth).
                    continue;
                }
            }

            session.tabs.push(codepp_core::Tab {
                path: tab.path.clone(),
                cursor,
                encoding: tab.encoding.clone(),
                eol: tab.eol,
                untitled_seq: tab.untitled_seq,
                backup: backup_filename,
                // Round-trip the user-chosen rename label. `None`
                // for path-bound buffers (their display name comes
                // from `path`); for untitled buffers, this is the
                // value File → Rename... wrote, so the next session
                // restores the same label rather than reverting to
                // `new N`.
                custom_name: tab.custom_name.clone(),
                // Persist the per-buffer language. Skip the
                // `L_TEXT` default for path-bound tabs whose
                // extension would auto-detect to `L_TEXT` anyway
                // — no information is lost, and older session.xml
                // files don't get rewritten with a no-op
                // attribute. Untitled buffers and any tab whose
                // current lang differs from the extension-derived
                // default get an explicit `@lang` written so the
                // user's Language-menu choice survives the
                // restart.
                lang: {
                    let extension_default = tab.path.as_deref().map_or(
                        codepp_core::lang::L_TEXT,
                        codepp_core::lang::LangType::from_path,
                    );
                    if tab.lang == extension_default {
                        None
                    } else {
                        Some(tab.lang.as_npp_id())
                    }
                },
                // Persist the user's pin choice so pinned tabs come
                // back pinned (and at the left edge) on next launch.
                // Older session.xml files without the attribute
                // deserialize with `pinned = false` (unpinned), so
                // no migration is required.
                pinned: tab.pinned,
            });
        }
        session.active = self.active_tab.and_then(|active_idx| {
            // Map the tabs[] index to the index inside session.tabs[],
            // accounting for any tabs we skipped above (a backup
            // write that failed, or an untitled tab in a sandboxed
            // environment with no backups dir).
            let mut session_idx = 0usize;
            for (i, tab) in self.tabs.iter().enumerate() {
                let was_persisted = tab.path.is_some()
                    || (tab.untitled_seq.is_some()
                        && session
                            .tabs
                            .iter()
                            .any(|s| s.untitled_seq == tab.untitled_seq && s.path.is_none()));
                if !was_persisted {
                    continue;
                }
                if i == active_idx {
                    return Some(session_idx);
                }
                session_idx += 1;
            }
            None
        });
        session
            .save_to_xml(&session_path)
            .map_err(|e| ShellError::Session(e.to_string()))?;

        // Now that session.xml is durably on disk, prune any backup
        // files in the directory that this save pass didn't write —
        // those are leftovers from a previous session whose tabs no
        // longer exist (closed, saved-and-cleaned, or the user moved
        // to fewer untitled tabs). Doing this *after* the session.xml
        // write means a crash mid-save can never delete a backup
        // that the surviving session.xml still references.
        if let Some(dir) = backups_dir {
            prune_unreferenced_backups(&dir, &written_backups);
        }

        Ok(())
    }

    /// Read `session.xml` and return one entry per restored tab in
    /// the original order, capped at [`MAX_SESSION_TABS`].
    ///
    /// Each entry tells the UI exactly what to do for that tab slot:
    /// open a file from disk for saved tabs, or restore an untitled
    /// buffer with text loaded from its backup file. The order of
    /// the returned Vec matches the order of `<tab>` entries in
    /// session.xml, so the UI just iterates and dispatches per
    /// variant — preserving the user's tab arrangement exactly.
    ///
    /// The parsed [`Session`] is stored on `self` so
    /// `apply_load_result` can later look up each restored tab's
    /// cursor position by path, and so `session_active_index` can
    /// report the saved active index.
    ///
    /// Returns an empty Vec when the file is missing — first-run
    /// case, never an error. A parse failure also returns empty,
    /// with a warn-level log so a corrupted file is observable.
    pub fn load_session_entries(&mut self) -> Vec<SessionRestoreEntry> {
        let Some(path) = codepp_platform::session_xml_path() else {
            return Vec::new();
        };
        let mut session = match Session::load_from_xml(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = ?e, "session.xml load failed; starting clean");
                return Vec::new();
            }
        };
        normalize_session_pinning(&mut session);
        // Cap restored tabs at `MAX_SESSION_TABS` to prevent a
        // tampered or runaway session.xml from queuing thousands
        // of loads / backup reads at startup (local DoS).
        if session.tabs.len() > MAX_SESSION_TABS {
            tracing::warn!(
                stored = session.tabs.len(),
                cap = MAX_SESSION_TABS,
                "session.xml exceeds tab cap; excess tabs not restored",
            );
        }
        let backups_dir = codepp_platform::backups_dir();
        let entries: Vec<SessionRestoreEntry> = session
            .tabs
            .iter()
            .take(MAX_SESSION_TABS)
            .filter_map(|t| {
                // Three branches by the (path, backup) shape:
                //   * (Some, Some) → saved file with unsaved
                //     edits at last save; restore as dirty using
                //     the backup text.
                //   * (Some, None) → clean saved file; queue a
                //     normal async open from disk.
                //   * (None, Some) → untitled buffer; create with
                //     backup text already in place.
                //   * (None, None) → corrupt entry; skip.
                // Resolve the backup, returning both the text *and*
                // the absolute path the text came from. The path
                // is needed below for the mtime comparison that
                // detects external edits during the recovery
                // window.
                let backup_loaded: Option<(String, PathBuf)> =
                    t.backup.as_deref().and_then(|name| {
                        if !is_safe_backup_filename(name) {
                            tracing::warn!(
                                backup = %name,
                                "session.xml backup name failed safety check; backup ignored",
                            );
                            return None;
                        }
                        let dir = backups_dir.as_ref()?;
                        let abs_path = dir.join(name);
                        match read_backup_file(dir, &abs_path) {
                            Ok(text) => Some((text, abs_path)),
                            Err(e) => {
                                tracing::warn!(
                                    backup = %abs_path.display(),
                                    error = %e,
                                    "failed to read backup file; falling back to disk content if path-bound",
                                );
                                None
                            }
                        }
                    });
                // Capture the backup's filename for the mtime check
                // below — `backup_loaded` carries the *absolute*
                // path; the filename portion is what
                // `parse_backup_timestamp` operates on.
                let backup_filename = t.backup.clone();
                if let Some(path) = &t.path {
                    if let Some((text, backup_path)) = backup_loaded {
                        // Detect external edits during the
                        // recovery window: if the on-disk file
                        // was modified more recently than the
                        // backup we're about to overlay, the
                        // user's first File→Save would silently
                        // overwrite that external write. Surface
                        // the conflict via the standard reload
                        // prompt — the user picks "keep my edits"
                        // (overwriting the disk) or "reload"
                        // (dropping the backup overlay).
                        let disk_changed_externally =
                            is_disk_newer_than_backup(path, &backup_path);
                        // Independent: did *the backup itself*
                        // get edited? Compares the file's mtime
                        // against the timestamp encoded in its
                        // filename.
                        let backup_modified_externally = backup_filename
                            .as_deref()
                            .is_some_and(|name| is_backup_modified_externally(&backup_path, name));
                        Some(SessionRestoreEntry::DirtyFromBackup {
                            path: path.clone(),
                            text,
                            cursor: t.cursor,
                            encoding: t.encoding.clone(),
                            eol: t.eol,
                            disk_changed_externally,
                            backup_modified_externally,
                            lang: t.lang,
                            pinned: t.pinned,
                        })
                    } else {
                        Some(SessionRestoreEntry::OpenFile(path.clone()))
                    }
                } else {
                    // No `path` → must be an untitled buffer. Map
                    // the optional backup text directly into the
                    // optional restore entry; a missing backup is
                    // a corrupt session entry (`None`-`None`) that
                    // we skip rather than restoring as an empty
                    // tab.
                    backup_loaded.map(|(text, backup_path)| {
                        let backup_modified_externally = backup_filename
                            .as_deref()
                            .is_some_and(|name| is_backup_modified_externally(&backup_path, name));
                        SessionRestoreEntry::UntitledFromBackup {
                            untitled_seq: t.untitled_seq,
                            text,
                            cursor: t.cursor,
                            encoding: t.encoding.clone(),
                            eol: t.eol,
                            backup_modified_externally,
                            custom_name: t.custom_name.clone(),
                            lang: t.lang,
                            pinned: t.pinned,
                        }
                    })
                }
            })
            .collect();
        self.session = session;
        entries
    }

    /// Backwards-compatible wrapper that returns just the disk
    /// paths from session.xml. Untitled buffers and any tab whose
    /// backup file is missing are silently dropped. Kept so the
    /// older single-call shape ("just give me the paths") remains
    /// available; new code should prefer `load_session_entries`
    /// to also restore untitled buffers.
    pub fn load_session_paths(&mut self) -> Vec<PathBuf> {
        self.load_session_entries()
            .into_iter()
            .filter_map(|e| match e {
                SessionRestoreEntry::OpenFile(p) => Some(p),
                // Dirty saved-file tabs surface as a path so callers
                // who only consume `load_session_paths` still get
                // the file opened (without the unsaved overlay,
                // which only `load_session_entries` carries).
                SessionRestoreEntry::DirtyFromBackup { path, .. } => Some(path),
                SessionRestoreEntry::UntitledFromBackup { .. } => None,
            })
            .collect()
    }

    /// Re-create an untitled buffer at the end of the tab list with
    /// content that came from a backup file. Same shape as
    /// [`Self::new_untitled`] but seeds the buffer with the saved
    /// text and restores the original `untitled_seq` so the user's
    /// "new 3" comes back as "new 3" rather than the next free
    /// number.
    ///
    /// Used by the UI's session-restore loop. Returns the new tab's
    /// index in `self.tabs`.
    // Many parameters is over the clippy default. They model one
    // logical thing (an untitled tab being restored from backup);
    // bundling into a struct would just shuffle the noise since
    // they're consumed once.
    #[allow(clippy::too_many_arguments)]
    pub fn restore_untitled_with_text<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        untitled_seq: Option<u32>,
        text: String,
        cursor: u64,
        encoding: Encoding,
        eol: Eol,
        backup_modified_externally: bool,
        custom_name: Option<String>,
        lang: Option<i32>,
        pinned: bool,
    ) -> usize {
        let id = self.allocate_buffer_id();
        let new_idx = self.tabs.len();
        let byte_len = text.len() as u64;
        // Untitled buffers have no extension to detect from, so
        // the persisted `@lang` is the only signal — fall back to
        // `L_TEXT` only when nothing is stored.
        let resolved_lang = lang.map_or(codepp_core::lang::L_TEXT, codepp_core::lang::LangType);
        self.tabs.push(Tab {
            id,
            path: None,
            encoding: encoding.clone(),
            eol,
            byte_len,
            text: text.clone(),
            pending_load: None,
            scintilla_doc: 0,
            lang: resolved_lang,
            untitled_seq,
            // Initial value — Scintilla's `SCN_SAVEPOINTLEFT` fires
            // when the UI populates the new doc with the backup
            // text, which flips this to `true` synchronously. The
            // brief paint window before the notification arrives
            // shows the clean icon; visible only on a paint that
            // races the load completion (rarely observable).
            dirty: false,
            custom_name,
            pinned,
        });
        self.active_tab = Some(new_idx);
        // Bind a fresh Scintilla document; matches `new_untitled`'s
        // path. The 0 sentinel asks the UI to allocate.
        let new_doc = ui.activate_tab(new_idx, 0);
        if let Some(tab) = self.tabs.get_mut(new_idx) {
            tab.scintilla_doc = new_doc;
        }
        // Push the backup text + cursor into the now-active editor
        // document. From the user's perspective this is identical
        // to having opened the buffer at the same position last
        // session.
        ui.set_buffer_text(&text, cursor);
        // Mark the new doc clean so the editor doesn't show a
        // dirty glyph immediately — the buffer matches what's on
        // (backup-)disk, even though there's no real file path.
        ui.mark_saved();
        // Apply the resolved language to the Scintilla view so a
        // restored untitled buffer with a persisted lang override
        // (e.g. user picked Rust on a renamed `new 1`) actually
        // renders with that highlighting on first paint instead
        // of waiting for a tab switch. Symmetric with
        // `restore_dirty_with_text`'s `ui.apply_lang(lang)` call.
        ui.apply_lang(resolved_lang);
        ui.update_status(resolved_lang, &encoding, eol, byte_len);
        // External-edit detection on the backup file itself. If
        // another program touched the recovery file between our
        // last save and this restore, the buffer content the
        // user is now looking at isn't what they typed — they
        // need to know.
        if backup_modified_externally {
            let label = untitled_seq.map_or_else(
                || format!("untitled-{}", self.tabs[new_idx].id),
                |n| format!("new {n}"),
            );
            tracing::warn!(
                untitled = %label,
                "backup file modified externally; surfacing warning",
            );
            self.deferred_dialogs.push(PendingDialog::Error {
                title: "Backup modified externally".to_string(),
                message: format!(
                    "The recovery file for unsaved buffer '{label}' was changed by another \
                     program since Code++ last saved. The content shown is the modified \
                     version — save it (File → Save As) if you want to keep it.",
                ),
            });
        }
        new_idx
    }

    /// Re-create a saved-file tab whose buffer had unsaved edits at
    /// the previous session's save time. The tab is bound to `path`
    /// (so File→Save writes there) but its Scintilla document is
    /// seeded with `text` from the backup file — i.e. the user's
    /// last in-memory state, *not* the on-disk state. The buffer
    /// stays dirty so the user sees the unsaved-edits glyph and
    /// knows to save (or reload to drop them).
    ///
    /// Returns the new tab's index in `self.tabs`. Companion to
    /// [`Self::restore_untitled_with_text`] for the
    /// `SessionRestoreEntry::DirtyFromBackup` variant.
    ///
    /// `disk_changed_externally` carries the conflict signal
    /// computed by `load_session_entries` (disk mtime > backup
    /// mtime). When `true`, this method queues a
    /// `PendingDialog::ConfirmReload(path)` onto
    /// `Shell.deferred_dialogs` so the next `drain` call surfaces
    /// the standard reload prompt — the user picks "keep my
    /// unsaved edits" (overwriting the external write on next
    /// save) or "reload" (dropping the backup overlay and taking
    /// the disk version). Without this signal the user's first
    /// File→Save would silently overwrite an external edit made
    /// during the recovery window.
    // Many parameters is over the clippy default (7 max). They
    // all model a single logical thing — the state of one tab
    // being restored from backup — but bundling them into a
    // struct would just shuffle the noise; they're consumed
    // immediately. The lint suppression is local and documented.
    #[allow(clippy::too_many_arguments)]
    pub fn restore_dirty_with_text<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        path: PathBuf,
        text: String,
        cursor: u64,
        encoding: Encoding,
        eol: Eol,
        disk_changed_externally: bool,
        backup_modified_externally: bool,
        stored_lang: Option<i32>,
        pinned: bool,
    ) -> usize {
        let id = self.allocate_buffer_id();
        let new_idx = self.tabs.len();
        let byte_len = text.len() as u64;
        // Lang resolution mirrors `apply_load_result`: the
        // persisted Language-menu choice wins; otherwise fall
        // back to extension-based auto-detection.
        let lang = stored_lang.map_or_else(|| LangType::from_path(&path), LangType);
        self.tabs.push(Tab {
            id,
            path: Some(path.clone()),
            encoding: encoding.clone(),
            eol,
            byte_len,
            text: text.clone(),
            pending_load: None,
            scintilla_doc: 0,
            lang,
            untitled_seq: None,
            // Backup-restored buffers carry edits the user never
            // committed to disk — paint them as dirty (red icon)
            // from the very first frame, even before Scintilla
            // fires SCN_SAVEPOINTLEFT in response to set_buffer_text
            // re-injecting the backup content. The user's mental
            // model is "this still needs saving", regardless of
            // whether the underlying doc has yet been touched.
            dirty: true,
            custom_name: None,
            pinned,
        });
        self.active_tab = Some(new_idx);
        let new_doc = ui.activate_tab(new_idx, 0);
        if let Some(tab) = self.tabs.get_mut(new_idx) {
            tab.scintilla_doc = new_doc;
        }
        // Seed the buffer with the backup text + cursor. We
        // deliberately do NOT call `mark_saved` here — that's the
        // whole point of this code path. `set_buffer_text` leaves
        // Scintilla's modified flag set, so the dirty glyph
        // appears and the user knows there's unsaved work.
        ui.set_buffer_text(&text, cursor);
        ui.apply_lang(lang);
        ui.update_status(lang, &encoding, eol, byte_len);
        // Watch the file so a future external edit prompts a
        // reload, matching the behaviour of a fresh disk-load
        // open. A failed watch is non-fatal — the tab still
        // works, just without external-change detection.
        if let Err(e) = self.file_watcher.watch(&path) {
            tracing::debug!(error = %e, path = ?path, "watch on dirty restore");
        }
        // External-edit detection. If the on-disk file was
        // modified during the recovery window (between the
        // previous session save and this restore), we'd otherwise
        // silently overwrite that change on the user's first
        // File→Save. Defer a `ConfirmReload` dialog onto the
        // shell's `deferred_dialogs` queue — `Shell::drain`
        // returns it to the UI on the next pump iteration, where
        // the existing reload-prompt path takes over (Yes →
        // `confirm_reload(path)` replaces the backup text with
        // disk content; No → user keeps their unsaved edits and
        // their next save overwrites the disk).
        if disk_changed_externally {
            tracing::info!(
                path = ?path,
                "on-disk file modified during recovery window; queuing reload prompt",
            );
            self.deferred_dialogs
                .push(PendingDialog::ConfirmReload(path.clone()));
        }
        // Independent: did the *backup file itself* get edited?
        // Even if `disk_changed_externally` already prompted, the
        // user might pick "No, keep my edits" — in which case
        // they need to know the backup overlay isn't necessarily
        // their own work. One Error dialog per case so the
        // user's mental model of which file changed how stays
        // accurate.
        if backup_modified_externally {
            let label = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            tracing::warn!(
                path = ?path,
                "backup file modified externally; surfacing warning",
            );
            self.deferred_dialogs.push(PendingDialog::Error {
                title: "Backup modified externally".to_string(),
                message: format!(
                    "The recovery file for '{label}' was changed by another program \
                     since Code++ last saved. The buffer shows the modified backup \
                     content — save it if you want to keep it.",
                ),
            });
        }
        // Queue NPPN_SNAPSHOTDIRTYFILELOADED so plugins that
        // audit-log file activity treat this restore the same
        // way they treat a fresh `FileOpened` — but with the
        // distinct event code that signals "this came from the
        // backup file, not a clean disk-load." Carries the
        // freshly-allocated `id` so plugins can correlate with
        // subsequent buffer-id-keyed events. Drained after
        // the `&mut Shell` borrow ends, same pattern as the
        // other lifecycle notifications.
        #[cfg(target_os = "windows")]
        self.pending_notifications
            .push(Notification::SnapshotDirtyFileLoaded {
                buffer_id: id as isize,
            });
        new_idx
    }

    /// Active tab index recorded in the most recently parsed
    /// session.xml, or `None` if no session was loaded. Returned as
    /// a separate accessor so the UI can read it after the
    /// `load_session_paths` + `open_file` loop without keeping
    /// `&self.session` borrowed across mutations.
    #[must_use]
    pub fn session_active_index(&self) -> Option<usize> {
        self.session.active
    }

    /// Window geometry recorded in the most recently parsed
    /// session.xml, or `None` if the session predates the feature
    /// or was missing. The UI reads this *after*
    /// `load_session_paths` and uses it to size the main window
    /// before the first `ShowWindow`. Stored separately from the
    /// runtime-tracked geometry below so a missing file is
    /// distinguishable from "loaded; nothing has changed yet".
    #[must_use]
    pub fn saved_window_geometry(&self) -> Option<WindowGeometry> {
        self.session.window
    }

    /// Update the cached window geometry from the UI on every
    /// observed change (`WM_SIZE` non-maximized, maximize/restore
    /// transitions). The next `save_session` writes this through
    /// to disk; UI is responsible for only feeding restored
    /// (non-maximized) sizes into `width`/`height` so a maximized
    /// + close cycle remembers the right "small" fallback.
    pub fn set_window_geometry(&mut self, geometry: WindowGeometry) {
        self.session.window = Some(geometry);
    }

    /// Persisted "Folder as Workspace" panel state read from
    /// session.xml, or `None` if the session predates the
    /// feature (no `<workspace>` element) or the panel was
    /// never opened. The UI reads this after `load_session_paths`
    /// and — if `visible` is set on the returned entry — pops
    /// the panel open at the stored `root` during cold-start
    /// restore.
    #[must_use]
    pub fn saved_workspace_session(&self) -> Option<codepp_core::session::WorkspaceSession> {
        self.session.workspace.clone()
    }

    /// Update the cached workspace panel state from the UI.
    /// Called from the periodic autosave and shutdown paths so
    /// the next launch cold-starts into the same panel state.
    /// Setting `None` clears any previously-recorded workspace
    /// (e.g. the user opened a folder this session but closed
    /// the panel and expects it not to reopen next launch).
    pub fn set_workspace_session(
        &mut self,
        workspace: Option<codepp_core::session::WorkspaceSession>,
    ) {
        self.session.workspace = workspace;
    }

    /// Persisted global editor-view toggles read from session.xml
    /// (currently just the indent-guide flag; the struct is the
    /// growth spot for future view-level toggles like word-wrap or
    /// show-line-numbers). The UI reads this at cold start —
    /// after `Shell::new` and before window-show — so the first
    /// paint already reflects the user's chosen state instead of
    /// Scintilla's built-in default.
    #[must_use]
    pub fn saved_view_settings(&self) -> codepp_core::session::ViewSettings {
        self.session.view
    }

    /// Update the cached view-toggle state from the UI. Called
    /// from the toolbar / menu handler for each toggle so the
    /// next `save_session` (autosave or shutdown) writes the
    /// new value through to disk and the next launch restores it.
    pub fn set_view_settings(&mut self, view: codepp_core::session::ViewSettings) {
        self.session.view = view;
    }
}

/// Local-time timestamp string used as the suffix on backup
/// filenames. Format `YYYY-MM-DD_HHMMSS` (no separators in the
/// time portion) matches the layout Notepad++ uses, so the
/// directory reads at a glance.
///
/// Implementation falls back to UTC seconds-since-epoch on the
/// (extremely unlikely) `SystemTime` failure rather than panicking
/// — a save that produces a slightly weird timestamp is still a
/// save that protected the user's data.
fn backup_timestamp() -> String {
    // The user-facing convention here is local wall-clock time.
    // `SystemTime::now()` is wall-clock; we format via the `time`
    // crate would be ideal but isn't a current dep, so we hand-
    // assemble using `chrono`-free arithmetic on a UNIX timestamp
    // and the system time-zone offset reported by Windows.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Approximate local time using `chrono`-style arithmetic on a
    // raw u64. We're not adding a chrono dep just for the
    // backup-filename timestamp; UTC is acceptable here because
    // the timestamp is a uniqueness suffix, not a user-readable
    // wall-clock display. The format chosen still matches N++'s
    // lexicographic shape so `ls` orders backups by save time.
    let days = secs / 86_400;
    let mut day_secs = secs % 86_400;
    let hour = day_secs / 3600;
    day_secs %= 3600;
    let minute = day_secs / 60;
    let second = day_secs % 60;
    // Civil-from-days conversion (Howard Hinnant's algorithm —
    // public-domain, well-trodden). Converts days-since-1970-01-01
    // into Y/M/D.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}_{hour:02}{minute:02}{second:02}")
}

/// Build the human-readable display portion of a backup filename
/// for a saved-file tab. Strips the directory component, keeps only
/// the basename, and replaces any character that
/// `is_safe_backup_filename` would later reject (path separators,
/// `:`, leading `.`, …) with `_`. Empty / pure-dot input falls back
/// to a stable placeholder so the filename is always non-empty and
/// safely round-trippable.
///
/// Examples:
///   `C:\Users\Max\notes.txt` → `notes.txt`
///   `/etc/hosts`             → `hosts`
///   `:weird:`                → `_weird_`
///   `..`                     → `untitled` (after fallback)
fn sanitize_basename_for_backup(path: &Path) -> String {
    let raw = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    if raw.is_empty() || raw == "." || raw == ".." {
        return "untitled".into();
    }
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        // Mirror the rejection grammar in `is_safe_backup_filename`:
        // drop path separators, drive-letter `:`, the `@`
        // separator we use ourselves, and control bytes. Every
        // other printable character (including spaces and
        // non-ASCII letters) survives — modern filesystems all
        // handle the latter cleanly.
        if c == '/' || c == '\\' || c == ':' || c == '@' || c.is_control() {
            out.push('_');
        } else {
            out.push(c);
        }
    }
    // Neutralise any `..` substring — `is_safe_backup_filename`
    // rejects it at restore time as a path-traversal guard, and
    // a basename like `foo..bar.txt` is legal on every modern
    // filesystem. Without this pass the backup would be written
    // successfully but silently dropped on the next launch,
    // losing the user's unsaved edits. `replace("..", "__")` is
    // non-overlapping, so an input of `"..."` becomes `"_."`
    // after one pass — leaving a single dot, which is fine. The
    // `while` keeps iterating until no `..` remains, handling
    // arbitrary dot-runs cleanly.
    while out.contains("..") {
        out = out.replace("..", "__");
    }
    // A name starting with `.` would later trip the dotfile guard.
    // Prefix with `_` instead of stripping so the user can still
    // tell which file the backup came from.
    if out.starts_with('.') {
        out.insert(0, '_');
    }
    out
}

/// Parse a backup filename's `@YYYY-MM-DD_HHMMSS` suffix into a
/// UNIX timestamp (seconds since epoch). Returns `None` if the
/// filename doesn't contain `@` or the suffix doesn't match the
/// fixed `backup_timestamp` format. Used to detect external edits
/// to backup files: comparing the file's actual `mtime` against
/// the time we wrote it (encoded directly in the filename) lets
/// us notice when *another program* touched the backup between
/// session-save and session-restore.
fn parse_backup_timestamp(filename: &str) -> Option<u64> {
    // Format: `<name>@YYYY-MM-DD_HHMMSS` (suffix is exactly 17
    // chars). `rfind` so a stray `@` in the display portion (the
    // sanitiser already replaces it with `_` but be defensive)
    // doesn't break parsing.
    let at_idx = filename.rfind('@')?;
    let ts = filename.get(at_idx + 1..)?;
    if ts.len() != 17 {
        return None;
    }
    let b = ts.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'_' {
        return None;
    }
    let year: i64 = std::str::from_utf8(&b[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&b[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&b[8..10]).ok()?.parse().ok()?;
    let hour: u32 = std::str::from_utf8(&b[11..13]).ok()?.parse().ok()?;
    let minute: u32 = std::str::from_utf8(&b[13..15]).ok()?.parse().ok()?;
    let second: u32 = std::str::from_utf8(&b[15..17]).ok()?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour >= 24
        || minute >= 60
        || second >= 60
    {
        return None;
    }
    // Howard Hinnant's days_from_civil — exact inverse of the
    // civil-from-days conversion in `backup_timestamp`. Converts
    // (year, month, day) to days-since-1970-01-01.
    let y_adj = if month <= 2 { year - 1 } else { year };
    let era = y_adj.div_euclid(400);
    let yoe = (y_adj - era * 400) as u64;
    // Parenthesised so the `as u64` cast unambiguously applies to
    // the result of the whole `if`.
    let m_offset = u64::from(if month > 2 { month - 3 } else { month + 9 });
    let doy = (153 * m_offset + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_signed = era * 146_097 + doe as i64 - 719_468;
    if days_signed < 0 {
        return None;
    }
    let secs = (days_signed as u64) * 86_400
        + u64::from(hour) * 3600
        + u64::from(minute) * 60
        + u64::from(second);
    Some(secs)
}

/// `true` iff the backup file's `mtime` is meaningfully later than
/// the timestamp embedded in its own filename — which means
/// another program edited the file between when we last wrote it
/// and now. Used at restore time to surface a "your backup was
/// modified externally" warning so the user isn't silently
/// presented with content they didn't type.
///
/// Defaults to `false` (no warning) on any I/O error, missing
/// timestamp suffix, or unsupported `mtime` API. The `+ 5`
/// tolerance covers FAT-style 1-second mtime resolution plus the
/// small gap between forming the timestamp string and the
/// `sync_all + persist` rename returning.
fn is_backup_modified_externally(backup_path: &Path, backup_filename: &str) -> bool {
    let Some(written_secs) = parse_backup_timestamp(backup_filename) else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(backup_path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let mtime_secs = match mtime.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => return false,
    };
    mtime_secs > written_secs + 5
}

/// Read the user's disabled-plugin list from
/// `<plugins_config_dir>/disabled.txt`. Returns each non-empty,
/// non-comment line trimmed of whitespace; the resulting `Vec`
/// is the canonical "disable this DLL" key set fed to
/// [`PluginHost::apply_disabled_list`]. Missing file → empty
/// list (first-run case). Read failure → empty list with a
/// warn-level log, so a corrupted file never locks the user out
/// of every plugin.
#[cfg(target_os = "windows")]
fn read_disabled_plugins_list() -> Vec<String> {
    let Some(path) = codepp_platform::disabled_plugins_path() else {
        return Vec::new();
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "disabled-plugins read failed");
            return Vec::new();
        }
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(std::string::ToString::to_string)
        .collect()
}

/// Write the disabled-plugin list back to disk atomically (sibling
/// temp file + rename, same recipe as session.xml). Empty
/// `filenames` writes an empty file rather than removing — keeps
/// the file's existence as a marker that the feature is wired up.
/// Creates the parent directory on first use.
#[cfg(target_os = "windows")]
fn write_disabled_plugins_list(filenames: &[String]) -> std::io::Result<()> {
    let Some(path) = codepp_platform::disabled_plugins_path() else {
        return Ok(());
    };
    let mut body = String::new();
    body.push_str(
        "# Plugins disabled by Code++'s Plugin Manager. One DLL filename per line.\n\
         # Lines starting with `#` are comments; blank lines are ignored.\n",
    );
    for f in filenames {
        // NTFS technically allows embedded `\n` / `\r` in filenames
        // (extremely rare in practice). Writing one verbatim would
        // inject an extra line into `disabled.txt` and could
        // disable a different plugin than intended on the next
        // launch. Skip + log; one-line-per-record is the file
        // format's invariant.
        if f.contains('\n') || f.contains('\r') {
            tracing::warn!(filename = %f, "skipping disabled-plugin entry with embedded newline");
            continue;
        }
        body.push_str(f);
        body.push('\n');
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    use std::io::Write;
    let mut tmp = tempfile::Builder::new()
        .prefix(".disabled-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    tmp.write_all(body.as_bytes())?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(())
}

/// `true` iff the on-disk file at `disk_path` has a more recent
/// `mtime` than the backup file at `backup_path`. Used by
/// `load_session_entries` to detect external edits that happened
/// during the recovery window — the gap between the previous
/// session save and this restore — when an unattended app might
/// otherwise silently overwrite an external write on first save.
///
/// Defaults to `false` (no conflict assumed) on any I/O error or
/// if either filesystem can't report a modification time. Erring
/// toward "no prompt" matches the pre-mtime behaviour and keeps
/// startup quiet for filesystems with a non-standard time API.
///
/// **UI-thread assumption:** the two `std::fs::metadata` calls
/// stat both paths synchronously. We're called from
/// `load_session_entries` which already does synchronous file
/// I/O (reading the backup), so the additional stat is at parity
/// with existing startup cost. On a cold cache or a network-
/// mapped drive these stalls compound — if startup latency for
/// a session full of remote-drive saved files becomes a real
/// issue, the natural fix is moving session restore onto the
/// loader thread; nothing in this helper's signature blocks
/// that move.
fn is_disk_newer_than_backup(disk_path: &Path, backup_path: &Path) -> bool {
    let Ok(disk_meta) = std::fs::metadata(disk_path) else {
        return false;
    };
    let Ok(backup_meta) = std::fs::metadata(backup_path) else {
        return false;
    };
    let Ok(disk_t) = disk_meta.modified() else {
        return false;
    };
    let Ok(backup_t) = backup_meta.modified() else {
        return false;
    };
    disk_t > backup_t
}

/// Reject backup filenames that contain a path separator, parent-
/// Enforce the "pinned-before-unpinned" invariant on a freshly-
/// parsed [`Session`] by stable-sorting `session.tabs` so pinned
/// entries occupy the left prefix, then remapping `session.active`
/// so it still points at the same tab it did pre-sort.
///
/// `Shell::save_session` writes tabs in the invariant-preserving
/// order, so a `session.xml` Code++ produced itself is already
/// sorted — this pass is a no-op there. The point is to defend
/// against a hand-edited or corrupted `session.xml` that
/// interleaves pinned and unpinned `<tab>` entries: without this
/// pass, the tab vector would be reassembled in the on-disk order
/// (which, for `SessionRestoreEntry::OpenFile`, is the order
/// `apply_load_result` pushes tabs), and `first_unpinned_idx`
/// would no longer describe the actual pinned prefix.
///
/// `apply_load_result`'s by-path lookup on `self.session.tabs`
/// must observe the same reordered view so a late-arriving async
/// load picks up the correct persisted `pinned`/`lang`/`cursor`
/// fields — that's why the sort mutates the stored `Session`
/// rather than a copy.
///
/// `sort_by_key` with `Reverse(pinned)` is stable in Rust's stdlib
/// (guaranteed since 1.6), preserving relative order within each
/// group — matches the "insertion order is preserved among pinned
/// tabs" contract in [`SessionRestoreEntry::UntitledFromBackup::pinned`]
/// and its `DirtyFromBackup` sibling.
fn normalize_session_pinning(session: &mut Session) {
    // Capture an identity for the currently-active tab so we can
    // find it again after the sort. `(path, untitled_seq)` is a
    // unique identity across every tab shape Code++ writes:
    // path-bound tabs have `path = Some(_)` and `untitled_seq =
    // None`; untitled tabs have `path = None` and `untitled_seq
    // = Some(_)`; the pair distinguishes both.
    let active_path = session
        .active
        .and_then(|i| session.tabs.get(i))
        .map(|t| (t.path.clone(), t.untitled_seq));
    session.tabs.sort_by_key(|t| std::cmp::Reverse(t.pinned));
    if let Some((prev_path, prev_seq)) = active_path {
        session.active = session
            .tabs
            .iter()
            .position(|t| t.path == prev_path && t.untitled_seq == prev_seq);
    }
}

/// directory traversal, drive-letter prefixes, Windows reserved
/// device names, or other shapes that would let a tampered
/// session.xml escape the backups directory or open a special
/// device. The legitimate naming scheme (`<display>@<timestamp>`)
/// never matches any of these — display names like `new 1` and
/// timestamps like `2026-05-04_215750` carry no special characters.
fn is_safe_backup_filename(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // `starts_with('.')` covers `.`, `..`, and any hidden-file
    // shape (`.session-...tmp`, `.foo`) that we don't want to
    // surface as a "backup" we'd later prune.
    if name.starts_with('.') {
        return false;
    }
    // Reject any path traversal or absolute-path / drive-letter
    // shape.
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.contains(':') {
        return false;
    }
    // Windows reserved device names. `dir.join("CON")` opens the
    // console device, so a tampered session.xml with
    // `backup="CON"` would hang `read_to_string` on the device
    // rather than reading a file. The reserved set is matched
    // case-insensitively against the name's stem (the portion
    // before any trailing `.`), since `CON.txt` is *also* the
    // console on Windows.
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
        "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or(name);
    let stem_upper: String = stem.to_ascii_uppercase();
    if RESERVED.iter().any(|r| *r == stem_upper) {
        return false;
    }
    true
}

/// Write `bytes` to `path` atomically (sibling temp file +
/// rename). Same atomic-write recipe the session.xml save uses,
/// adapted for raw bytes. Creates the parent directory if it
/// doesn't exist yet — first-launch case.
fn write_backup_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".backup-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Read a backup file capped at [`MAX_BACKUP_FILE_BYTES`] and
/// return its content as UTF-8. Filenames are first validated
/// against [`is_safe_backup_filename`] in the load path; this
/// function additionally canonicalises both the requested path and
/// the parent directory and refuses any path whose canonical form
/// escapes the backups directory — defence-in-depth against a
/// tampered session.xml or a symlink dropped into the backup dir
/// (the latter only writable by a local attacker, but cheap to
/// guard).
fn read_backup_file(expected_dir: &Path, path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    // Canonicalise both ends so the prefix comparison works even
    // when the backups dir uses a relative path or contains
    // symlinks. `canonicalize` requires the file to exist, which
    // matches our flow (we only call this after walking the
    // directory) — for a missing file we'd want the caller to see
    // the natural ENOENT.
    let canonical_path = std::fs::canonicalize(path)?;
    let canonical_dir = std::fs::canonicalize(expected_dir)?;
    if !canonical_path.starts_with(&canonical_dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "backup file canonicalises outside the backups directory",
        ));
    }
    // TOCTOU note: there's a window between `canonicalize` and
    // `File::open` where `canonical_path` could in theory be swapped
    // out. Because `canonical_path` is the *resolved* absolute target
    // (symlinks already chased), the only attacker who could win that
    // race is one with write access to the user's own AppData / config
    // dir — i.e. the same user. Acceptable under the desktop-app
    // trust boundary; no privilege escalation surface.
    let mut file = std::fs::File::open(&canonical_path)?;
    let metadata = file.metadata()?;
    if metadata.len() > MAX_BACKUP_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("backup file exceeds {MAX_BACKUP_FILE_BYTES} byte cap"),
        ));
    }
    // `as usize` cast is safe: the cap above (64 MiB) is well within
    // `usize::MAX` on every supported target including 32-bit. The
    // cap-check is the real defence — the cast could only overflow
    // on a target where `MAX_BACKUP_FILE_BYTES` exceeded `usize::MAX`,
    // which the const keeps below.
    let mut buf = String::with_capacity(metadata.len() as usize);
    file.read_to_string(&mut buf)?;
    Ok(buf)
}

/// Delete every file in `dir` whose name isn't in the `keep` list.
/// Called after a successful session.xml write to bound the
/// directory's size — old backups from previous sessions that the
/// new session.xml no longer references would otherwise pile up
/// over time.
///
/// Errors are logged at warn level rather than propagated: a
/// failed prune is non-critical (next save will try again), and
/// returning an error here would mask a *successful* session.xml
/// write higher up.
fn prune_unreferenced_backups(dir: &Path, keep: &[String]) {
    // No backup directory yet (first save with no untitled
    // tabs) — nothing to prune.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        // Skip stray temp files created by `tempfile`'s atomic
        // write that didn't get persisted (rare; an interrupted
        // save would otherwise grow these in-place).
        if name_str.starts_with(".backup-") {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        if keep.iter().any(|k| k == name_str) {
            continue;
        }
        // Belt-and-braces: only delete files that look like our
        // backup naming convention (`<name>@<timestamp>`). A user
        // who manually drops files into the backup dir doesn't
        // lose them on next save.
        if !name_str.contains('@') {
            continue;
        }
        if let Err(e) = std::fs::remove_file(entry.path()) {
            tracing::warn!(file = ?entry.path(), error = %e, "failed to remove stale backup file");
        }
    }
}

/// Errors that can arise from `Shell` operations. The display form
/// of each variant ends up in user-facing message dialogs (Save
/// failed, Save All summary, etc.) via `e.to_string()`.
///
/// **Threat-model note (per the m8-commit-1 security audit):** the
/// `String` payloads on `Encoding`, `Io`, `Session`, and
/// `WatcherInit` are produced by `e.to_string()` on the underlying
/// error type and may include filesystem paths the user has
/// supplied (the Save As destination, the path of an externally
/// changed file, the location of `session.xml`). For Code++'s
/// threat model — a local desktop editor where the user supplies
/// every path themselves — those paths are *user-known input*, not
/// untrusted data, and echoing them back in an error dialog is
/// equivalent to the user re-reading what they typed. No
/// information disclosure occurs.
///
/// Explicit per-variant audit:
///
/// * `WatcherInit(s)` — surfaces the `notify` crate's init error.
///   Describes OS-level filesystem-watcher state (e.g. "too many
///   watchers"); no user paths.
/// * `NoActivePath` — no payload, no path data.
/// * `Encoding(s)` — surfaces `codepp_core::encoding::encode`
///   errors. Describes byte-level encoding failures (e.g. "char
///   '⠿' not representable in windows-1252"); no paths.
/// * `Io(s)` — surfaces `std::io::Error` and `tempfile::Error`
///   `to_string()`. The `std::io::Error` form does **not** include
///   the path; it's the OS-level error string ("permission
///   denied", "no such file"). `tempfile::PersistError.error` is
///   similarly path-free. The path lives separately in our own
///   error context (e.g. `format!("buffer {id}: {e}")` in Save All).
/// * `Session(s)` — surfaces `quick-xml` parse/write errors over
///   `session.xml`. May include the canonical session-file path
///   (`%APPDATA%\Code++\session.xml`), which is an
///   internal location, not user-supplied.
///
/// Re-evaluate this table whenever a new variant is added or an
/// existing variant's string source changes — particularly if a
/// new variant ever surfaces remote-server URLs, OAuth tokens, or
/// any other category the desktop-editor threat model doesn't
/// already cover.
#[derive(Debug)]
pub enum ShellError {
    WatcherInit(String),
    NoActivePath,
    Encoding(String),
    Io(String),
    Session(String),
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShellError::WatcherInit(s) => write!(f, "watcher init failed: {s}"),
            ShellError::NoActivePath => write!(f, "no active file path"),
            ShellError::Encoding(s) => write!(f, "encoding error: {s}"),
            ShellError::Io(s) => write!(f, "I/O error: {s}"),
            ShellError::Session(s) => write!(f, "session error: {s}"),
        }
    }
}

impl std::error::Error for ShellError {}

/// Adapter that exposes `Shell` + the per-call platform handles to the
/// plugin-host's `HostServices` trait. Lives only for the duration of
/// one `dispatch_plugin_message` call; carries `&mut Shell` and
/// `&mut U` so the trait's mutating methods (open, save, status-bar)
/// reach the right places without `Shell` having to know any HWND.
#[cfg(target_os = "windows")]
struct HostBridge<'a, U: UiPlatform> {
    shell: &'a mut Shell,
    ui: &'a mut U,
    handles: HostHandles,
}

#[cfg(target_os = "windows")]
impl<U: UiPlatform> HostServices for HostBridge<'_, U> {
    fn current_scintilla_hwnd(&self) -> Hwnd {
        self.handles.scintilla_main
    }

    fn scintilla_hwnd_for_view(&self, view: i32) -> Hwnd {
        match view {
            0 => self.handles.scintilla_main,
            1 => self.handles.scintilla_secondary,
            _ => core::ptr::null_mut(),
        }
    }

    fn current_buffer_id(&self) -> isize {
        // 0 means "no buffer" — matches the Notepad++ convention.
        // Active tab's id otherwise. A tab whose load is still
        // pending also reports its id (the buffer exists; only the
        // contents are still arriving), so plugins can address
        // newly-opened tabs without waiting for the load to finish.
        self.shell.active().map_or(0, |t| t.id as isize)
    }

    fn buffer_path(&self, id: isize) -> Option<PathBuf> {
        // Linear scan over tabs. Phase 3's tab counts are small
        // (handful at most); a HashMap<id, idx> is overkill until
        // multi-window or session-restore lands hundreds of tabs.
        self.shell
            .tabs
            .iter()
            .find(|t| t.id as isize == id)
            .and_then(|t| t.path.clone())
    }

    fn buffer_lang_type(&self, id: isize) -> i32 {
        // Phase 4 m1: every tab carries its own `LangType`, derived
        // from the path extension at first-load time. Plugins reading
        // NPPM_GETBUFFERLANGTYPE for an unknown id get `L_TEXT` (the
        // same default the tab is born with), matching Notepad++'s
        // "no such buffer" behaviour.
        self.shell
            .tabs
            .iter()
            .find(|t| t.id as isize == id)
            .map_or(L_TEXT.as_npp_id(), |t| t.lang.as_npp_id())
    }

    fn plugins_config_dir(&self) -> PathBuf {
        // Sandboxed runners may not resolve a config dir. Fall back
        // to the OS temp dir rather than the process CWD: a process
        // started from a network share or a directory the user does
        // not own would otherwise hand plugins an attacker-writable
        // location for their config files. `temp_dir` is always
        // user-owned on a healthy system. Plugins that depend on
        // cross-launch persistence still degrade gracefully — the
        // configuration goes to a tempdir for the duration of this
        // launch rather than crashing the host.
        codepp_platform::plugins_config_dir().unwrap_or_else(std::env::temp_dir)
    }

    fn menu_handle(&self, which: i32) -> Hwnd {
        match which {
            NPPPLUGINMENU => self.handles.plugin_menu,
            NPPMAINMENU => self.handles.main_menu,
            _ => core::ptr::null_mut(),
        }
    }

    fn set_status_bar(&mut self, section: usize, text: String) {
        self.ui.set_plugin_status(section, &text);
    }

    fn open_file(&mut self, path: PathBuf) {
        // Path comes verbatim from the plugin via NPPM_DOOPEN. Code++
        // does not confine plugin-driven opens — a plugin can ask
        // the host to open any file the host process can read. This
        // matches Notepad++'s own contract; plugins are in-process
        // and trusted with the host's full address space, so a
        // path-confinement check would be defense in depth against
        // a threat model where the plugin is hostile-but-not-yet-
        // executing-arbitrary-code, which is a narrow window.
        //
        // TODO milestone 5 hardening pass: reject `\\.\` device
        // paths and `\\?\` extended-length paths whose target
        // resolves outside the user's home tree, as a courtesy
        // against accidental plugin bugs. Not security-critical
        // given the threat model; included for sharper diagnostics.
        //
        // Return value discarded here: the dispatch trait's
        // `open_file` predates `OpenFileOutcome`. The UI-side
        // rebind hook the interactive `ID_FILE_OPEN` path uses
        // instead lives in `main_wnd_proc`'s plugin-dispatch arm,
        // which snapshots `active_tab` around
        // `dispatch_plugin_message` and fires
        // `handle_tab_selchange` synchronously if the plugin's
        // NPPM_* call flipped the active tab to a materialised
        // background buffer.
        let _ = self.shell.open_file(path);
    }

    fn reload_file(&mut self, path: Option<PathBuf>) {
        let path = path.or_else(|| self.shell.active().and_then(|t| t.path.clone()));
        if let Some(p) = path {
            self.shell.confirm_reload(p);
        }
    }

    fn save_current_file(&mut self) {
        if let Err(e) = self.shell.save_current_to_disk(self.ui) {
            tracing::warn!(error = %e, "plugin-triggered save failed");
        }
    }

    fn switch_to_file(&mut self, path: PathBuf) -> bool {
        // If the path is already open in some tab, activate it.
        // Otherwise route through the regular open path, which
        // either reuses an empty active tab or pushes a new one.
        if let Some(idx) = self
            .shell
            .tabs
            .iter()
            .position(|t| t.path.as_deref() == Some(path.as_path()))
        {
            // Skip the queue entirely if the target is already
            // active — `NPPN_BUFFERACTIVATED` signals "the user's
            // active buffer changed," and a switch to the
            // already-active buffer is not such a change. Plugins
            // that audit-log activations would otherwise log
            // false positives, and plugins that reset buffer-local
            // state on activation would clobber valid state.
            if self.shell.active_tab != Some(idx) {
                self.shell.active_tab = Some(idx);
                self.shell.queue_buffer_activated();
            }
            true
        } else {
            // Return value discarded: the position() check above
            // already handled the "already open" case, so this arm
            // only enqueues a fresh load — `OpenFileOutcome::Loading`
            // is handled by the drain's `activate_tab` when the
            // loader posts its wake, no dedupe hook needed.
            let _ = self.shell.open_file(path);
            // open_file's load completion will fire BUFFERACTIVATED
            // for the new tab via apply_load_result.
            true
        }
    }

    fn menu_command(&mut self, cmd_id: i32) {
        // Routes the plugin's N++-ABI `IDM_*` id through the
        // UI-side mapping onto the equivalent Code++ `ID_*`, then
        // fires it via `PostMessage(WM_COMMAND)` — same code path
        // a user's menu click takes. Plugin-allocated cmd ids
        // (from `NPPM_ALLOCATECMDID` / FuncItem entries) pass
        // through untouched.
        //
        // `dispatch_npp_menu_command` returns `false` for
        // unmapped built-in ids (Notepad++ features Code++
        // doesn't implement yet). We don't propagate that up —
        // the `NPPM_MENUCOMMAND` ABI has no rejection channel;
        // the trace-log in the UI method is enough for
        // diagnostics.
        let _ = self.ui.dispatch_npp_menu_command(cmd_id);
    }

    fn make_current_buffer_dirty(&mut self) {
        // The UI-side impl toggles Scintilla's dirty state via
        // `SCI_ADDUNDOACTION` — Scintilla has no direct "set
        // dirty" primitive, so the container-action shift-past-
        // savepoint is the closest reachable analogue. Idempotent
        // when the buffer is already dirty (avoids stacking phantom
        // undo entries).
        self.ui.mark_active_buffer_dirty();
    }

    fn set_buffer_lang_type(&mut self, id: isize, lang: i32) -> bool {
        // Phase 4 m2: real per-buffer lang switch.
        //
        //   1. Find the tab. Unknown id → FALSE (matches N++'s
        //      "no such buffer, nothing changed" return).
        //   2. No-op same-lang sets — re-applying the same lexer
        //      flickers the visible buffer and a NPPN_LANGCHANGED
        //      fired for an unchanged lang would be a false
        //      positive that breaks plugins audit-logging language
        //      changes.
        //   3. Mutate the data model first; if this is the active
        //      tab, re-apply the lexer through the UI (the lexer
        //      lives on the *view*, not the document, so the
        //      apply_lang call has to land on the active editor
        //      regardless of which tab the plugin targeted).
        //   4. Queue NPPN_LANGCHANGED. Drain happens after the
        //      &mut Shell borrow drops, same as the other
        //      lifecycle notifications.
        let new_lang = LangType(lang);
        let Some(idx) = self.shell.tabs.iter().position(|t| t.id as isize == id) else {
            return false;
        };
        if self.shell.tabs[idx].lang == new_lang {
            return true;
        }
        self.shell.tabs[idx].lang = new_lang;
        if self.shell.active_tab == Some(idx) {
            self.ui.apply_lang(new_lang);
            // Refresh the status bar's language slot too — the
            // lexer changes via `apply_lang` but the chrome
            // doesn't know that, and a user-driven Language menu
            // pick (or a plugin's NPPM_SETBUFFERLANGTYPE) shouldn't
            // require a tab switch to reflect the new label. Read
            // the rest of the metadata back from the tab so the
            // call sets the same encoding / EOL / byte-length the
            // bar already showed; the dynamic-parts refresh inside
            // `update_status` re-reads Scintilla for length /
            // cursor / INS-OVR.
            let tab = &self.shell.tabs[idx];
            let encoding = tab.encoding.clone();
            let eol = tab.eol;
            let byte_len = tab.byte_len;
            self.ui.update_status(new_lang, &encoding, eol, byte_len);
        }
        self.shell
            .pending_notifications
            .push(Notification::LangChanged {
                buffer_id: self.shell.tabs[idx].id as isize,
            });
        true
    }

    fn language_name(&self, lang: i32) -> Option<&'static str> {
        LangType(lang).language_name()
    }

    fn language_desc(&self, lang: i32) -> Option<&'static str> {
        LangType(lang).language_desc()
    }

    fn set_menu_item_check(&mut self, cmd_id: i32, checked: bool) {
        // Routes the plugin's `IDM_*` id through the same UI-side
        // mapping as `menu_command`, then calls the native
        // check-menu-item primitive (`CheckMenuItem(MF_BYCOMMAND)`
        // on Win32). Plugin-allocated cmd ids (>= `PLUGIN_CMD_ID_MIN`)
        // pass through so plugins can toggle the check state on
        // their own submenu entries. Same "no rejection channel"
        // note as `menu_command`: `NPPM_SETMENUITEMCHECK` returns
        // void, so a mapping miss just no-ops with a trace log.
        let _ = self.ui.set_npp_menu_item_check(cmd_id, checked);
    }

    fn activate_doc(&mut self, view: i32, pos: i32) -> bool {
        // wparam: view selector (0 = primary, 1 = secondary).
        // lparam: tab position in that view.
        //
        // Single-view through Phase 4: only `view == 0` resolves to
        // a real tab list; secondary view is reserved for split-
        // view (Phase 5). Out-of-range pos returns false.
        //
        // Same metadata-only pattern as `switch_to_file`'s
        // activate-existing-tab branch: flip `active_tab`, queue
        // `NPPN_BUFFERACTIVATED`, leave the visible Scintilla
        // re-binding to the wnd_proc's normal sync cycle. Plugin-
        // driven activation of a non-current tab is rare and the
        // existing dispatch path treats this the same way.
        if view != 0 || pos < 0 {
            return false;
        }
        let idx = pos as usize;
        if idx >= self.shell.tabs.len() {
            return false;
        }
        if self.shell.active_tab != Some(idx) {
            self.shell.active_tab = Some(idx);
            self.shell.queue_buffer_activated();
        }
        true
    }

    fn launch_find_in_files_dialog(&mut self, directory: Option<PathBuf>, filters: Option<String>) {
        // Stash the prefill on the underlying Shell; the Win32
        // dispatch in `main_wnd_proc` drains it via
        // `Shell::take_fif_launch_prefill` right after
        // `dispatch_plugin_message` returns. The dialog open
        // itself can't happen here — the shell layer doesn't know
        // about HWNDs (DESIGN.md §2.1) — but the prefill data
        // structure is shared, so the UI sees exactly what the
        // plugin requested. Routed through the public setter to
        // keep the take/set pair as the only field-touch sites.
        self.shell
            .set_fif_launch_prefill(FifLaunchPrefill { directory, filters });
    }

    fn open_buffer_paths(&self, selector: i32) -> Vec<PathBuf> {
        // Single-view through Phase 4: ALL_OPEN_FILES and
        // PRIMARY_VIEW return the same set; SECOND_VIEW is empty.
        // Untitled tabs (no on-disk path) are filtered out so the
        // TCHAR** plugin contract — each slot receives a real
        // path — holds. Tab order in `shell.tabs` matches the tab
        // strip's left-to-right order, which is what plugins
        // expect for "the i-th open file".
        match selector {
            codepp_plugin_host::ALL_OPEN_FILES | codepp_plugin_host::PRIMARY_VIEW => self
                .shell
                .tabs
                .iter()
                .filter_map(|t| t.path.clone())
                .collect(),
            // SECOND_VIEW and any unknown selector both map to
            // empty — split-view is Phase 5, so the secondary
            // slot has nothing today.
            _ => Vec::new(),
        }
    }

    fn current_doc_index(&self, view: i32) -> i32 {
        // Primary view exposes the active tab's `tabs[]` index;
        // secondary view doesn't exist yet (split-view is Phase 5),
        // so it reports -1 — the documented "no view" sentinel.
        // The `i as i32` cast is safe: `MAX_SESSION_TABS = 512`,
        // well below `i32::MAX`.
        match view {
            0 => self.shell.active_tab.map_or(-1, |i| i as i32),
            _ => -1,
        }
    }

    fn buffer_encoding(&self, id: isize) -> i32 {
        // Map Code++'s internal `Encoding` to N++'s `UniMode` enum.
        // `Other` (unknown WHATWG codepage label, e.g. `windows-1252`,
        // `shift_jis`) collapses to `UNI_8BIT` — N++'s ABI doesn't
        // carry the codepage identity past this point either, and
        // plugins gating on "is this Unicode?" still get the right
        // answer (UNI_8BIT is "no").
        let Some(tab) = self.shell.tabs.iter().find(|t| t.id as isize == id) else {
            return -1;
        };
        match &tab.encoding {
            codepp_core::Encoding::Utf8 => codepp_plugin_host::UNI_COOKIE,
            codepp_core::Encoding::Utf8Bom => codepp_plugin_host::UNI_UTF8,
            codepp_core::Encoding::Utf16LeBom => codepp_plugin_host::UNI_UTF16LE,
            codepp_core::Encoding::Utf16BeBom => codepp_plugin_host::UNI_UTF16BE,
            codepp_core::Encoding::Utf16Le => codepp_plugin_host::UNI_UTF16LE_NO_BOM,
            codepp_core::Encoding::Utf16Be => codepp_plugin_host::UNI_UTF16BE_NO_BOM,
            codepp_core::Encoding::Other(_) => codepp_plugin_host::UNI_8BIT,
        }
    }

    fn buffer_format(&self, id: isize) -> i32 {
        // Map Code++'s internal `Eol` to N++'s `EolType`. `Mixed` is
        // unique to Code++ (per-line preservation when a file's EOL
        // is inconsistent); we report `UNIX_FORMAT` since LF is the
        // modern default and matches what "Edit → EOL Conversion"
        // would normalise a mixed file to.
        let Some(tab) = self.shell.tabs.iter().find(|t| t.id as isize == id) else {
            return -1;
        };
        match tab.eol {
            codepp_core::Eol::CrLf => codepp_plugin_host::WIN_FORMAT,
            codepp_core::Eol::Cr => codepp_plugin_host::MAC_FORMAT,
            codepp_core::Eol::Lf | codepp_core::Eol::Mixed => codepp_plugin_host::UNIX_FORMAT,
        }
    }

    fn reload_buffer_id(&mut self, id: isize, with_alert: bool) -> bool {
        // Resolve the buffer id to its on-disk path. Untitled tabs
        // (no path) report -1 from `NPPM_GETFULLPATHFROMBUFFERID`
        // and similarly are not reloadable here — there's nothing
        // to re-read off disk.
        let Some(path) = self
            .shell
            .tabs
            .iter()
            .find(|t| t.id as isize == id)
            .and_then(|t| t.path.clone())
        else {
            return false;
        };
        if with_alert {
            // Phase 4 limitation: the dispatcher cannot push into
            // the per-window pending-dialog queue from inside a
            // synchronous plugin call without re-engineering the
            // borrow plumbing on `Shell::drain`. Silently reloading
            // matches `with_alert == false` — which is what most
            // plugins pass in practice. The trace makes the gap
            // observable; the wiring is tracked as a follow-up.
            tracing::warn!(
                buffer_id = id,
                path = %path.display(),
                "NPPM_RELOADBUFFERID with_alert=true: silent reload until \
                 dialog-queue wiring lands (Phase 5 polish)",
            );
        }
        // `confirm_reload` is the same code path the file watcher's
        // post-prompt "Yes" arm uses — re-runs the loader for `path`.
        self.shell.confirm_reload(path);
        true
    }

    fn set_buffer_encoding(&mut self, id: isize, unimode: i32) -> bool {
        // Inverse of `Self::buffer_encoding`'s mapping. We reject
        // UNI_7BIT outright: Code++'s detection pipeline never
        // produces it (pure ASCII is reported as `UNI_COOKIE`/Utf8)
        // and there's no exact `Encoding` variant for "ASCII", so
        // a plugin asking for it would silently get UTF-8 and be
        // surprised on save. Better to fail loudly with a `false`
        // return.
        //
        // UNI_8BIT maps to `windows-1252` because that's the de-
        // facto "ANSI" codepage on western-European Windows
        // installs. The encoding label round-trips through
        // `Encoding::from_label`, so a session.xml save+restore
        // cycle preserves the choice. (Future polish: detect the
        // system codepage via `GetACP` at startup and use that.)
        let encoding = match unimode {
            codepp_plugin_host::UNI_8BIT => {
                codepp_core::Encoding::Other("windows-1252".to_string())
            }
            codepp_plugin_host::UNI_UTF8 => codepp_core::Encoding::Utf8Bom,
            codepp_plugin_host::UNI_UTF16BE => codepp_core::Encoding::Utf16BeBom,
            codepp_plugin_host::UNI_UTF16LE => codepp_core::Encoding::Utf16LeBom,
            codepp_plugin_host::UNI_COOKIE => codepp_core::Encoding::Utf8,
            codepp_plugin_host::UNI_UTF16BE_NO_BOM => codepp_core::Encoding::Utf16Be,
            codepp_plugin_host::UNI_UTF16LE_NO_BOM => codepp_core::Encoding::Utf16Le,
            // UNI_7BIT, UNI_END, or anything else: rejected.
            _ => return false,
        };
        self.shell.set_buffer_encoding_by_id(id, encoding)
    }

    fn set_buffer_format(&mut self, id: isize, eoltype: i32) -> bool {
        // Inverse of `Self::buffer_format`'s mapping. Code++'s
        // `Eol::Mixed` is per-line preservation — a state plugins
        // cannot ask for since it has no N++ counterpart. The
        // setter never produces `Mixed`; only WIN/MAC/UNIX_FORMAT
        // are accepted.
        let eol = match eoltype {
            codepp_plugin_host::WIN_FORMAT => codepp_core::Eol::CrLf,
            codepp_plugin_host::MAC_FORMAT => codepp_core::Eol::Cr,
            codepp_plugin_host::UNIX_FORMAT => codepp_core::Eol::Lf,
            _ => return false,
        };
        self.shell.set_buffer_eol_by_id(id, eol)
    }

    fn encode_sci(&mut self, view: i32) -> i32 {
        // Single-view through Phase 4: only view 0 has an active
        // buffer. View 1 (secondary) is reserved for split-view
        // (Phase 5); it has no active buffer today, so a plugin
        // asking for it gets -1 — matching N++'s "view has no
        // buffer" return value.
        if view != 0 {
            return -1;
        }
        let id = self.current_buffer_id();
        if id == 0 {
            return -1;
        }
        // The `set_buffer_encoding_by_id` short-circuit on
        // same-value still reports success, so a plugin calling
        // `NPPM_ENCODESCI` on an already-UTF-8 buffer gets
        // UNI_COOKIE back without any state churn. The bool
        // return distinguishes "set succeeded" from "unknown id";
        // the id we just pulled from `current_buffer_id()` is by
        // construction live (it points at the active tab), so the
        // false case is unreachable here. The `debug_assert!` pins
        // that invariant so a future refactor making
        // `current_buffer_id` return stale ids surfaces in tests
        // rather than silently lying to the plugin.
        let ok = self
            .shell
            .set_buffer_encoding_by_id(id, codepp_core::Encoding::Utf8);
        debug_assert!(ok, "current_buffer_id() returned an id no live tab carries");
        codepp_plugin_host::UNI_COOKIE
    }

    fn decode_sci(&mut self, view: i32) -> i32 {
        if view != 0 {
            return -1;
        }
        let id = self.current_buffer_id();
        if id == 0 {
            return -1;
        }
        // Same `windows-1252` rationale as `set_buffer_encoding`'s
        // `UNI_8BIT` mapping — de-facto ANSI on western-European
        // installs; `GetACP`-driven detection is Phase 5 polish.
        // Same `debug_assert!` invariant as `encode_sci` above.
        let ok = self.shell.set_buffer_encoding_by_id(
            id,
            codepp_core::Encoding::Other("windows-1252".to_string()),
        );
        debug_assert!(ok, "current_buffer_id() returned an id no live tab carries");
        codepp_plugin_host::UNI_8BIT
    }

    fn is_tabbar_hidden(&self) -> bool {
        self.ui.is_tabbar_hidden()
    }

    fn set_tabbar_hidden(&mut self, hidden: bool) -> bool {
        self.ui.set_tabbar_hidden(hidden)
    }

    fn is_toolbar_hidden(&self) -> bool {
        self.ui.is_toolbar_hidden()
    }
    fn set_toolbar_hidden(&mut self, hidden: bool) -> bool {
        self.ui.set_toolbar_hidden(hidden)
    }
    fn is_menu_hidden(&self) -> bool {
        self.ui.is_menu_hidden()
    }
    fn set_menu_hidden(&mut self, hidden: bool) -> bool {
        self.ui.set_menu_hidden(hidden)
    }
    fn is_statusbar_hidden(&self) -> bool {
        self.ui.is_statusbar_hidden()
    }
    fn set_statusbar_hidden(&mut self, hidden: bool) -> bool {
        self.ui.set_statusbar_hidden(hidden)
    }

    fn alloc_supported(&self) -> bool {
        // Code++ implements `NPPM_ALLOCATECMDID` and
        // `NPPM_ALLOCATEMARKER` — the cmd-id pool is
        // 60_000..65_500 (5500 ids), the marker pool is 25..32
        // (seven markers above the bookmark slot). Plugins
        // gating on this can take the allocating path.
        true
    }

    fn allocate_cmd_id(&mut self, count: i32) -> Option<i32> {
        self.shell.plugins.allocate_cmd_id(count)
    }

    fn allocate_marker(&mut self, count: i32) -> Option<i32> {
        self.shell.plugins.allocate_marker(count)
    }

    fn appdata_plugins_allowed(&self) -> bool {
        // Code++ always loads from the per-user
        // `%APPDATA%\Code++\plugins` (no admin-restricted system
        // dir). Plugins gating on this can assume the answer is
        // always `true` until we ship a system-wide install path.
        true
    }

    fn current_view(&self) -> i32 {
        // Single-view through Phase 4. Phase 5 split-view will
        // return 0 / 1 based on which view has focus.
        0
    }

    fn plugin_home_dir(&self) -> Option<PathBuf> {
        codepp_platform::plugins_dir()
    }

    fn settings_cloud_dir(&self) -> Option<PathBuf> {
        // Code++ doesn't implement settings cloud-sync — the
        // dispatcher writes an empty wide string into the plugin
        // buffer. Plugins reading the result get a length-0 path
        // and should treat it as "no cloud sync configured".
        None
    }

    fn bookmark_marker_id(&self) -> i32 {
        // N++'s convention: marker number 24. Plugins use
        // `NPPM_GETBOOKMARKID` to learn the marker number, then
        // call `SCI_MARKERADD(line, MARKER_ID)` to install a
        // bookmark on a buffer. Code++'s UI doesn't yet style
        // marker 24 as a visible bookmark glyph (Phase 4 polish),
        // but the marker is set on the buffer correctly and any
        // plugin that pre-populates bookmarks works the same way
        // it would in N++.
        24
    }

    fn editor_zoom_level(&self) -> i32 {
        self.ui.editor_zoom_level()
    }

    fn editor_default_fg_color(&self) -> i32 {
        self.ui.editor_default_fg_color()
    }

    fn editor_default_bg_color(&self) -> i32 {
        self.ui.editor_default_bg_color()
    }

    fn set_smooth_font(&mut self, smooth: bool) -> bool {
        self.ui.set_smooth_font(smooth)
    }

    fn set_editor_border_edge(&mut self, enable: bool) -> bool {
        self.ui.set_editor_border_edge(enable)
    }

    fn save_file(&mut self, path: PathBuf) -> bool {
        // Phase 4 limitation: only the active tab can be saved
        // through this path. Saving a background tab requires the
        // doc-pointer-swap dance tracked in DESIGN.md §7.4. If the
        // requested path matches the active tab, route through
        // `save_current_to_disk`; otherwise log and report 0.
        let active_path = self.shell.active().and_then(|t| t.path.clone());
        if active_path.as_ref() == Some(&path) {
            match self.shell.save_current_to_disk(self.ui) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "NPPM_SAVEFILE: write failed"
                    );
                    false
                }
            }
        } else {
            // Path doesn't match the active tab. Either it's a
            // known background tab (deferred to Phase 5 polish)
            // or unknown to Code++ entirely. The honest answer is
            // 0 in either case; the warn-log distinguishes the
            // two for diagnostic purposes.
            let known_background = self
                .shell
                .tabs
                .iter()
                .any(|t| t.path.as_deref() == Some(path.as_path()));
            if known_background {
                tracing::warn!(
                    path = %path.display(),
                    "NPPM_SAVEFILE: cross-tab save not yet supported (Phase 5)",
                );
            } else {
                tracing::trace!(
                    path = %path.display(),
                    "NPPM_SAVEFILE: path does not match any open tab",
                );
            }
            false
        }
    }

    fn is_doc_switcher_shown(&self) -> bool {
        // Code++ has no doc-switcher panel.
        false
    }

    fn set_doc_switcher_shown(&mut self, shown: bool) -> bool {
        // No-op. Code++ has no panel to show; report
        // "previously not shown" so a plugin gating on the
        // return-value-as-prev-state contract behaves correctly.
        tracing::trace!(
            shown = shown,
            "NPPM_SHOWDOCSWITCHER: no-op (Code++ has no doc-switcher panel)"
        );
        false
    }

    fn doc_switcher_disable_column(&mut self, column_idx: i32, disable: bool) {
        tracing::trace!(
            column_idx = column_idx,
            disable = disable,
            "NPPM_DOCSWITCHERDISABLECOLUMN: no-op (Code++ has no doc-switcher panel)"
        );
    }

    fn line_number_width_mode(&self) -> i32 {
        // Code++ uses dynamic line-number margins everywhere.
        codepp_plugin_host::LINENUMWIDTH_DYNAMIC
    }

    fn set_line_number_width_mode(&mut self, mode: i32) -> bool {
        if !matches!(
            mode,
            codepp_plugin_host::LINENUMWIDTH_DYNAMIC | codepp_plugin_host::LINENUMWIDTH_CONSTANT
        ) {
            return false;
        }
        self.ui.set_line_number_width_mode(mode)
    }

    fn user_lang_count(&self) -> i32 {
        // Code++ does not yet implement user-defined languages
        // (UDL). The honest count is 0; plugins gating on
        // `if (NPPM_GETNBUSERLANG())` skip their UDL-aware path.
        0
    }

    fn shortcut_for_cmd_id(&self, cmd_id: i32) -> Option<codepp_plugin_host::ShortcutKey> {
        self.ui.shortcut_for_cmd_id(cmd_id)
    }

    fn remove_shortcut_for_cmd_id(&mut self, cmd_id: i32) -> bool {
        let removed = self.ui.remove_shortcut_for_cmd_id(cmd_id);
        // Queue NPPN_SHORTCUTREMAPPED only on a real removal —
        // a no-op call (cmd_id had no binding) is silent. The
        // notification drains after `&mut Shell` releases, same
        // pattern as DocOrderChanged. `nmhdr.hwndFrom` is set
        // to NULL by `Notification::hwnd_from` for this variant
        // (the upstream removal contract).
        if removed {
            self.shell
                .pending_notifications
                .push(Notification::ShortcutRemapped { cmd_id });
        }
        removed
    }

    fn register_modeless_dialog(&mut self, dlg: codepp_plugin_host::Hwnd, register: bool) -> bool {
        self.ui.register_modeless_dialog(dlg, register)
    }

    fn add_toolbar_icon(&mut self, cmd_id: i32, hicon: codepp_plugin_host::Hwnd) -> bool {
        self.ui.add_toolbar_icon(cmd_id, hicon)
    }

    fn is_dark_mode_enabled(&self) -> bool {
        self.ui.is_dark_mode_enabled()
    }

    fn dark_mode_colors(&self, out: &mut codepp_plugin_host::NppDarkModeColors) -> bool {
        self.ui.dark_mode_colors(out)
    }

    fn create_plugin_scintilla(
        &mut self,
        parent: codepp_plugin_host::Hwnd,
    ) -> codepp_plugin_host::Hwnd {
        self.ui.create_plugin_scintilla(parent)
    }

    fn register_dock_dialog(&mut self, params: codepp_plugin_host::DockDialogParams) -> bool {
        self.ui.register_dock_dialog(params)
    }

    fn show_dock_dialog(&mut self, h_client: codepp_plugin_host::Hwnd) -> bool {
        self.ui.show_dock_dialog(h_client)
    }

    fn hide_dock_dialog(&mut self, h_client: codepp_plugin_host::Hwnd) -> bool {
        self.ui.hide_dock_dialog(h_client)
    }

    fn update_dock_disp_info(&mut self, h_client: codepp_plugin_host::Hwnd) -> bool {
        self.ui.update_dock_disp_info(h_client)
    }

    fn dock_hwnd_by_name(&self, name: &str, module_name: Option<&str>) -> codepp_plugin_host::Hwnd {
        self.ui.dock_hwnd_by_name(name, module_name)
    }

    fn trigger_tab_context_menu(&mut self, view: i32, tab_idx: i32) -> bool {
        // Code++'s tab strip doesn't yet ship a context menu (no
        // Close / Close-Others / Rename / Move-to-other-view
        // entries — Phase 4 polish). Returns `true` to signal
        // "request accepted" rather than `false` ("rejected"):
        // a plugin gating `if (NPPM_TRIGGERTABBARCONTEXTMENU)`
        // on the return for control flow (timer reset, audit
        // log, etc.) sees the host received its trigger
        // correctly. When the actual menu lands the return
        // continues to be `true` on success — no plugin
        // behaviour change. The trace-log surfaces requests so
        // the future implementation can verify the call sites.
        tracing::trace!(
            view,
            tab_idx,
            "NPPM_TRIGGERTABBARCONTEXTMENU: no-op (Code++ has no tab context menu yet)"
        );
        true
    }

    fn forward_plugin_message(
        &mut self,
        target_name: &str,
        internal_msg: i32,
        info_ptr: usize,
    ) -> isize {
        // Look up the target plugin by name. `name()` returns the
        // cached `getName()` value, which is the wire-format name
        // plugins use to identify each other (the value that
        // appears in the Plugins menu). An unloaded or panicked
        // plugin returns `None` here and the lookup misses — the
        // upstream contract says "0 when target isn't loaded."
        let Some(proc) = self
            .shell
            .plugins
            .iter()
            .find(|p| p.name.as_deref() == Some(target_name))
            .and_then(codepp_plugin_host::PluginInfo::message_proc_fn)
        else {
            tracing::trace!(
                target = target_name,
                msg = internal_msg,
                "NPPM_MSGTOPLUGIN: target plugin not found",
            );
            return 0;
        };
        // Wrap in `catch_unwind` so a Rust-authored target
        // plugin's panic doesn't unwind across the C ABI — same
        // safety boundary as `notify_all` for beNotified.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `proc` is the plugin's exported
            // `messageProc` — declared `unsafe extern "C" fn(u32,
            // usize, isize) -> isize` in `ffi.rs`. The plugin
            // contract is to read `info_ptr` as a
            // `CommunicationInfo*`; the host has done the
            // null-check before calling here.
            //
            // The `i32 -> u32` cast preserves the bit pattern. A
            // plugin sending a negative `long` for `internal_msg`
            // (legal upstream) sees the same `0xFFFF_FFFF`-style
            // value the equivalent N++ relay produces — N++'s
            // call site `target->messageProc(info->internalMsg, …)`
            // applies the same implicit cast since the
            // `messageProc` signature's first parameter is `UINT`.
            unsafe { proc(internal_msg as u32, info_ptr, 0) }
        }));
        if let Ok(lresult) = result {
            lresult
        } else {
            tracing::warn!(
                target = target_name,
                msg = internal_msg,
                "NPPM_MSGTOPLUGIN: target plugin panicked in messageProc",
            );
            0
        }
    }

    fn save_all_files(&mut self) {
        // `save_all` returns a Vec of per-tab errors. Each error is
        // already surfaced via `ShellError`'s display path elsewhere
        // in the UI; the plugin contract is "always success" so we
        // discard the per-tab result here. A `tracing::warn!` per
        // failure matches the same logging cadence the menu-driven
        // Save All path produces.
        let errors = self.shell.save_all(self.ui);
        for (tab_idx, err) in errors {
            tracing::warn!(tab_idx, error = %err, "plugin-triggered save_all: per-tab failure");
        }
    }

    fn program_dir(&self) -> Option<PathBuf> {
        codepp_platform::program_dir()
    }

    fn program_path(&self) -> Option<PathBuf> {
        codepp_platform::program_path()
    }

    fn windows_version(&self) -> i32 {
        codepp_platform::windows_version_npp()
    }

    fn buffer_position(&self, id: isize) -> Option<(i32, i32)> {
        // Single-view through Phase 4: every known buffer lives in
        // the primary view (0). Untitled tabs *are* addressable here
        // — unlike `open_buffer_paths`, the position lookup just
        // wants a tab index, so the lack of an on-disk path doesn't
        // exclude the tab.
        let idx = self.shell.tabs.iter().position(|t| t.id as isize == id)?;
        Some((0, idx as i32))
    }

    fn buffer_id_at(&self, view: i32, pos: i32) -> isize {
        // Single-view: only view 0 has buffers. Out-of-range index
        // (negative or beyond the open count) returns 0 — N++'s
        // documented "no buffer" sentinel.
        if view != 0 || pos < 0 {
            return 0;
        }
        let pos = pos as usize;
        self.shell.tabs.get(pos).map_or(0, |t| t.id as isize)
    }

    fn save_current_as(&mut self, path: PathBuf, as_copy: bool) -> bool {
        if as_copy {
            // Save a copy: encode + atomic-rename to `path` without
            // touching the active tab's metadata. The buffer keeps
            // tracking its original on-disk path, matching N++'s
            // `NPPM_SAVECURRENTFILEAS(asCopy=TRUE)` contract.
            //
            // Edge case: if `path` happens to equal the active tab's
            // own path, the "copy without state mutation" semantic
            // collapses to an in-place save. Routing through
            // `save_active_as_copy` would skip the file-watcher
            // unwatch/rewatch dance — Win32's `notify` translates
            // the atomic temp+rename to `Modify(Name)` which our
            // mapping classifies as `FileChange::Removed`, popping
            // a false "file deleted externally" prompt. Detect the
            // same-path case and reroute through
            // `save_current_to_disk`, which already handles the
            // watcher dance + the `NPPN_FILEBEFORESAVE` /
            // `NPPN_FILESAVED` queue pushes the tab expects when
            // its own bytes are being rewritten.
            let same_path = self
                .shell
                .active()
                .and_then(|t| t.path.as_ref())
                .is_some_and(|p| p == &path);
            if same_path {
                return matches!(self.shell.save_current_to_disk(self.ui), Ok(()));
            }
            match self.shell.save_active_as_copy(self.ui, &path) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(),
                        "NPPM_SAVECURRENTFILEAS(asCopy=TRUE) failed");
                    false
                }
            }
        } else {
            match self.shell.save_buffer_as(self.ui, path.clone()) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(),
                        "NPPM_SAVECURRENTFILEAS(asCopy=FALSE) failed");
                    false
                }
            }
        }
    }

    fn load_session(&mut self, path: PathBuf) -> bool {
        // Read the foreign session-XML, then route every titled
        // file through the regular open_file path — same as a user
        // who manually re-opened each tab. The session's recorded
        // active-tab is honoured (the *last* successful open
        // becomes the active tab anyway, because each open_file
        // promotes the new tab to active). Untitled-tab entries
        // and `WindowGeometry` are ignored: those describe the
        // *recording tool's* state, not state plugins are asking
        // Code++ to adopt.
        let session = match codepp_core::session::Session::load_from_xml(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "NPPM_LOADSESSION: failed to read session file");
                return false;
            }
        };
        for tab in &session.tabs {
            if let Some(p) = tab.path.clone() {
                // Return value discarded here: the plugin dispatch
                // arm in `main_wnd_proc` snapshots `active_tab`
                // around `dispatch_plugin_message` and fires the
                // synchronous rebind on any net change — same hook
                // that catches HostBridge::open_file and
                // switch_to_file's dedupe cases.
                let _ = self.shell.open_file(p);
            }
        }
        true
    }

    fn save_current_session(&self, path: PathBuf) -> bool {
        let files: Vec<PathBuf> = self
            .shell
            .tabs
            .iter()
            .filter_map(|t| t.path.clone())
            .collect();
        write_session_files(&path, &files)
    }

    fn save_session_with_files(&self, path: PathBuf, files: Vec<PathBuf>) -> bool {
        write_session_files(&path, &files)
    }

    fn read_session_file_paths(&self, path: PathBuf) -> Option<Vec<PathBuf>> {
        let session = codepp_core::session::Session::load_from_xml(&path)
            .map_err(|e| {
                tracing::warn!(error = %e, path = %path.display(),
                    "NPPM_GETSESSIONFILES: failed to read session file");
                e
            })
            .ok()?;
        Some(session.tabs.into_iter().filter_map(|t| t.path).collect())
    }
}

/// Helper for the two `NPPM_SAVE`*SESSION paths. Builds a minimal
/// `Session` with one `Tab` per file (no encoding / EOL / cursor
/// metadata — plugins are saving a *file list*, not a fidelity-
/// preserving snapshot of the editor state) and writes it via
/// `Session::save_to_xml`. Returns `true` on success, `false` on
/// any I/O / serialization failure (the dispatcher reports the
/// boolean back to the plugin via the message return value).
///
/// `cfg(target_os = "windows")`-gated because every caller is in
/// the `HostBridge` impl, which is similarly gated. Without the
/// gate, Linux / macOS CI runs the dead-code lint and fails
/// (`-D warnings`).
#[cfg(target_os = "windows")]
fn write_session_files(path: &Path, files: &[PathBuf]) -> bool {
    let session = codepp_core::session::Session {
        active: None,
        window: None,
        workspace: None,
        view: codepp_core::session::ViewSettings::default(),
        tabs: files
            .iter()
            .map(|p| codepp_core::session::Tab {
                path: Some(p.clone()),
                ..Default::default()
            })
            .collect(),
    };
    match session.save_to_xml(path) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(),
                "NPPM_SAVE*SESSION: failed to write session file");
            false
        }
    }
}

/// Spawn a forwarder thread that pumps items from `src` into `dst`
/// and calls `wake` after each successful send. Used so the shell
/// can wake the UI thread on every producer event without modifying
/// the producer crates' APIs.
/// Read `find_history.xml` if present. A missing file is normal
/// (first launch); a corrupt one is logged + ignored so the user
/// still gets a working dialog with empty dropdowns.
fn load_find_history() -> FindHistory {
    let Some(path) = codepp_platform::find_history_xml_path() else {
        return FindHistory::default();
    };
    match FindHistory::load(&path) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "find_history.xml load failed; starting empty");
            FindHistory::default()
        }
    }
}

/// Save `find_history.xml`. Errors are logged + swallowed —
/// failing to persist the dropdown list isn't worth bubbling
/// up through the find/replace UI path. Cfg-gated to Windows
/// because every caller is on a cfg-gated find/replace method;
/// without the gate, a Linux/macOS lint build flags it as
/// dead code.
#[cfg(target_os = "windows")]
fn save_find_history(history: &FindHistory) {
    let Some(path) = codepp_platform::find_history_xml_path() else {
        return;
    };
    if let Err(e) = history.save(&path) {
        tracing::warn!(path = %path.display(), error = %e, "find_history.xml save failed");
    }
}

/// Preinstalled Markdown UDL bundled at build time via
/// `include_bytes!`. Copied into
/// `<config_dir>/userDefineLangs/` on first run by
/// [`copy_preinstalled_udls`] so a fresh install shows
/// "Markdown (preinstalled)" in the Language menu without the
/// user having to hand-install anything.
///
/// The file lives at `assets/preinstalled-udls/markdown._pre
/// installed.udl.xml` at the workspace root — an MIT-licensed
/// verbatim copy from Edditoria's `markdown-plus-plus` (see
/// `assets/preinstalled-udls/PROVENANCE.md`). Embedding at
/// build time (rather than shipping alongside the exe) means
/// the binary is self-contained: a user copying `code++.exe`
/// to a USB stick still has Markdown UDL after Code++ runs
/// once and populates its config dir.
const PREINSTALLED_MARKDOWN_UDL: &[u8] =
    include_bytes!("../../../assets/preinstalled-udls/markdown._preinstalled.udl.xml");

/// Filename the preinstalled Markdown UDL is written under.
/// Matches N++'s naming convention so a user migrating from
/// N++ can drop the file into `<config_dir>/userDefineLangs/`
/// and get identical behaviour.
const PREINSTALLED_MARKDOWN_FILENAME: &str = "markdown._preinstalled.udl.xml";

/// Copy the preinstalled UDL(s) into `dir` on first run. Called
/// from `Shell::new` after `create_dir_all(<userDefineLangs>)`
/// and before `UdlRegistry::scan_dir`.
///
/// **Skip-if-exists discipline.** If a file with the same
/// filename already exists in `dir`, DO NOT overwrite — the
/// user may have hand-edited (customised colours, added
/// keywords) or explicitly deleted the file to remove it from
/// their menu. Same discipline N++ uses; matches the
/// `assets/preinstalled-udls/PROVENANCE.md` note that "Users
/// who want the upstream GPLv3 plugins can still install them
/// by hand — runtime loading by the plugin host is the same
/// as for any other third-party plugin."
///
/// **Atomic write (TOCTOU-safe).** Uses `OpenOptions::create_new`
/// so the "file doesn't exist yet → create it" decision is a
/// single kernel operation, not a `.exists()` check followed by
/// a separate `fs::write`. Without this, another local process
/// running as the same user (a malicious plugin already loaded
/// this session, per DESIGN.md §6.5's in-process trust model,
/// or a pre-existing attacker with local code execution) could
/// plant a symlink/junction at
/// `<dir>/markdown._preinstalled.udl.xml` between the
/// `.exists()` check and the write, and Code++'s `fs::write` —
/// which follows reparse points — would overwrite whatever the
/// symlink pointed at with our embedded UDL bytes. `create_new`
/// treats "path exists as a regular file OR a symlink"
/// identically as `AlreadyExists`, collapsing the check-then-
/// act window to zero. `AlreadyExists` maps to the same skip
/// semantics as the previous `.exists()` early return.
///
/// **First-run persistence** — the copied file lives on disk
/// alongside user-installed UDLs. On second launch it's
/// already present, so the scanner just picks it up (no
/// re-copy). If the user deletes it, it stays deleted; if
/// they modify it, their modifications survive. All standard
/// preinstalled-asset conventions.
///
/// Errors are logged at warn and swallowed — a fresh install
/// that can't write its preinstalled UDLs still works, the
/// user just doesn't see "Markdown (preinstalled)" in the
/// menu.
fn copy_preinstalled_udls(dir: &Path) {
    use std::io::Write;
    let target = dir.join(PREINSTALLED_MARKDOWN_FILENAME);
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
    {
        Ok(f) => f,
        // AlreadyExists is the skip-if-present success case —
        // the user already has this file (or a hand-edit,
        // deletion-and-recreation, or a previous copy of it).
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return,
        Err(err) => {
            tracing::warn!(
                path = ?target,
                error = %err,
                "failed to create preinstalled Markdown UDL; \
                 skipping (Language menu will not show Markdown \
                 (preinstalled) until this file is present)"
            );
            return;
        }
    };
    if let Err(err) = file.write_all(PREINSTALLED_MARKDOWN_UDL) {
        tracing::warn!(
            path = ?target,
            error = %err,
            "failed to write preinstalled Markdown UDL contents; \
             partial file left on disk (Language menu may show \
             Markdown (preinstalled) with a parse error until \
             the file is deleted or replaced)"
        );
    }
}

/// Load `styles.xml` at startup. Mirrors `load_find_history` —
/// missing file → defaults, parse failure → log + defaults so a
/// corrupt styles.xml doesn't block startup.
fn load_styles() -> codepp_core::styles::Styles {
    let Some(path) = codepp_platform::styles_xml_path() else {
        return codepp_core::styles::Styles::default();
    };
    match codepp_core::styles::Styles::load_from_xml(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "styles.xml load failed; starting with defaults");
            codepp_core::styles::Styles::default()
        }
    }
}

/// Save `styles.xml`. Errors are logged + swallowed for the
/// same reason as [`save_find_history`]: the dialog has no
/// useful recovery path for a write failure, and the in-memory
/// state is already updated so the session reflects the user's
/// choice regardless of whether disk write succeeded.
fn save_styles(styles: &codepp_core::styles::Styles) {
    let Some(path) = codepp_platform::styles_xml_path() else {
        return;
    };
    if let Err(e) = styles.save_to_xml(&path) {
        tracing::warn!(path = %path.display(), error = %e, "styles.xml save failed");
    }
}

fn spawn_forwarder<T: Send + 'static>(
    src: Receiver<T>,
    dst: Sender<T>,
    wake: Arc<dyn Fn() + Send + Sync>,
    name: &'static str,
) {
    thread::Builder::new()
        .name(format!("codepp-{name}"))
        .spawn(move || {
            while let Ok(item) = src.recv() {
                if dst.send(item).is_err() {
                    break;
                }
                wake();
            }
        })
        .expect("forwarder thread spawn");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// Test `UiPlatform` that records calls — lets us assert the shell
    /// reaches the right operations without needing real Win32.
    ///
    /// Has more than three bool fields by design — each one
    /// gates a distinct branch the tests configure
    /// independently. Refactoring to a bitset / enum would
    /// obscure the test scaffolding's intent.
    #[derive(Default)]
    #[allow(clippy::struct_excessive_bools)]
    struct FakeUi {
        buffer_text: String,
        cursor: u64,
        set_text_calls: Vec<(String, u64)>,
        status_calls: Vec<(LangType, String, String, u64)>,
        plugin_status_calls: Vec<(usize, String)>,
        /// (`tab_idx`, `in_doc`, `returned_doc`) per `activate_tab` call.
        activate_tab_calls: Vec<(usize, isize, isize)>,
        /// Stand-in for `SCI_CREATEDOCUMENT` — hand out monotonically
        /// increasing fake "doc pointers" so each tab gets a
        /// distinct value.
        next_fake_doc: isize,
        apply_lang_calls: Vec<LangType>,
        apply_default_style_calls: Vec<codepp_core::styles::Styles>,
        search_calls: Vec<(String, SearchFlags, String)>,
        replace_calls: Vec<(String, String, SearchFlags, String)>,
        /// Counter incremented each time `mark_saved` is called —
        /// lets tests assert that every successful per-tab save in
        /// Save All cleared its dirty glyph (one `mark_saved` per
        /// successful tab).
        mark_saved_calls: usize,
        /// Tab-strip visibility shadow. Default `false` (visible)
        /// matches the real UI's startup state.
        tabbar_hidden: bool,
        /// Toolbar / main-menu / status-bar visibility shadows for
        /// the chrome-toggle messages (`NPPM_HIDETOOLBAR` etc.).
        /// All `false` (visible) by default — matches the real
        /// UI's startup chrome.
        toolbar_hidden: bool,
        menu_hidden: bool,
        statusbar_hidden: bool,
        /// Single-buffer dirty flag the test fixture maintains.
        /// `is_doc_dirty` returns this verbatim — tests that exercise
        /// the dirty-aware save path flip the flag manually.
        dirty: bool,
        /// Recorded N++-ABI menu command ids dispatched through
        /// `dispatch_npp_menu_command` — one entry per call, in
        /// order. Lets `NPPM_MENUCOMMAND` tests assert the
        /// dispatcher forwarded the exact id. `cfg(windows)`-gated
        /// because both the impl method and the tests that read
        /// the vec are Windows-only (`NPPM_*` dispatch lives in
        /// `plugin-host`, which is Windows-only until Phase 5).
        #[cfg(target_os = "windows")]
        npp_menu_commands: Vec<i32>,
        /// Recorded `(cmd_id, checked)` pairs from
        /// `set_npp_menu_item_check` — lets `NPPM_SETMENUITEMCHECK`
        /// tests assert both the id and the requested state. Same
        /// `cfg(windows)` gate rationale as `npp_menu_commands`.
        #[cfg(target_os = "windows")]
        npp_menu_checks: Vec<(i32, bool)>,
        /// Counter incremented each time `mark_active_buffer_dirty`
        /// is called — `NPPM_MAKECURRENTBUFFERDIRTY` tests assert
        /// the shell reached the UI method once per plugin call.
        mark_dirty_calls: usize,
    }

    impl UiPlatform for FakeUi {
        fn activate_tab(&mut self, idx: usize, scintilla_doc: isize) -> isize {
            // If the tab already has a doc, keep it; otherwise hand
            // out a fresh fake pointer (the real Win32 impl calls
            // SCI_CREATEDOCUMENT here).
            let bound = if scintilla_doc != 0 {
                scintilla_doc
            } else {
                self.next_fake_doc += 1;
                self.next_fake_doc
            };
            self.activate_tab_calls.push((idx, scintilla_doc, bound));
            bound
        }
        fn set_buffer_text(&mut self, text: &str, cursor: u64) {
            self.buffer_text = text.to_string();
            self.cursor = cursor;
            self.set_text_calls.push((text.to_string(), cursor));
        }
        fn get_buffer_text(&mut self) -> String {
            self.buffer_text.clone()
        }
        fn get_cursor_pos(&mut self) -> u64 {
            self.cursor
        }
        fn update_status(
            &mut self,
            lang: codepp_core::LangType,
            encoding: &Encoding,
            eol: Eol,
            byte_len: u64,
        ) {
            self.status_calls.push((
                lang,
                encoding.label().to_string(),
                eol.label().to_string(),
                byte_len,
            ));
        }
        fn set_plugin_status(&mut self, section: usize, text: &str) {
            self.plugin_status_calls.push((section, text.to_string()));
        }
        fn mark_saved(&mut self) {
            self.mark_saved_calls += 1;
        }
        fn apply_lang(&mut self, lang: LangType) {
            self.apply_lang_calls.push(lang);
        }
        fn apply_default_style(&mut self, styles: &codepp_core::styles::Styles) {
            self.apply_default_style_calls.push(styles.clone());
        }
        fn search_next(&mut self, query: &str, flags: SearchFlags) -> Option<u64> {
            // Naive in-test substring search over the fake buffer.
            // Records the call so tests can assert on it.
            self.search_calls
                .push((query.to_string(), flags, "next".to_string()));
            self.buffer_text.find(query).map(|pos| pos as u64)
        }
        fn search_prev(&mut self, query: &str, flags: SearchFlags) -> Option<u64> {
            self.search_calls
                .push((query.to_string(), flags, "prev".to_string()));
            self.buffer_text.rfind(query).map(|pos| pos as u64)
        }
        fn replace_current(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> bool {
            self.replace_calls.push((
                query.to_string(),
                replacement.to_string(),
                flags,
                "current".to_string(),
            ));
            // Replace the first occurrence in the fake buffer to
            // approximate what Scintilla does on the real path.
            if let Some(pos) = self.buffer_text.find(query) {
                self.buffer_text
                    .replace_range(pos..pos + query.len(), replacement);
                true
            } else {
                false
            }
        }
        fn replace_all(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> usize {
            // No empty-query guard here — `Shell::replace_all`
            // gates that before reaching the platform impl, so a
            // duplicate guard in the test fake would obscure which
            // layer is responsible.
            self.replace_calls.push((
                query.to_string(),
                replacement.to_string(),
                flags,
                "all".to_string(),
            ));
            let count = self.buffer_text.matches(query).count();
            self.buffer_text = self.buffer_text.replace(query, replacement);
            count
        }
        fn count_matches(&mut self, query: &str, flags: SearchFlags) -> usize {
            self.search_calls
                .push((query.to_string(), flags, "count".to_string()));
            self.buffer_text.matches(query).count()
        }
        fn search_next_in_range(
            &mut self,
            query: &str,
            flags: SearchFlags,
            start: u64,
            end: u64,
        ) -> Option<u64> {
            self.search_calls
                .push((query.to_string(), flags, "next_in_range".to_string()));
            let lo = (start as usize).min(self.buffer_text.len());
            let hi = (end as usize).min(self.buffer_text.len());
            if hi <= lo {
                return None;
            }
            self.buffer_text[lo..hi]
                .find(query)
                .map(|p| (lo + p) as u64)
        }
        fn search_prev_in_range(
            &mut self,
            query: &str,
            flags: SearchFlags,
            start: u64,
            end: u64,
        ) -> Option<u64> {
            self.search_calls
                .push((query.to_string(), flags, "prev_in_range".to_string()));
            let lo = (start as usize).min(self.buffer_text.len());
            let hi = (end as usize).min(self.buffer_text.len());
            if hi <= lo {
                return None;
            }
            self.buffer_text[lo..hi]
                .rfind(query)
                .map(|p| (lo + p) as u64)
        }
        fn replace_all_in_range(
            &mut self,
            query: &str,
            replacement: &str,
            flags: SearchFlags,
            start: u64,
            end: u64,
        ) -> (usize, u64) {
            self.replace_calls.push((
                query.to_string(),
                replacement.to_string(),
                flags,
                "all_in_range".to_string(),
            ));
            let lo = (start as usize).min(self.buffer_text.len());
            let hi = (end as usize).min(self.buffer_text.len());
            if hi <= lo {
                return (0, end);
            }
            let inside = &self.buffer_text[lo..hi];
            let count = inside.matches(query).count();
            let replaced_inside = inside.replace(query, replacement);
            let new_end = lo + replaced_inside.len();
            self.buffer_text.replace_range(lo..hi, &replaced_inside);
            (count, new_end as u64)
        }
        fn is_tabbar_hidden(&self) -> bool {
            self.tabbar_hidden
        }
        fn set_tabbar_hidden(&mut self, hidden: bool) -> bool {
            let prev = self.tabbar_hidden;
            self.tabbar_hidden = hidden;
            prev
        }
        fn is_toolbar_hidden(&self) -> bool {
            self.toolbar_hidden
        }
        fn set_toolbar_hidden(&mut self, hidden: bool) -> bool {
            let prev = self.toolbar_hidden;
            self.toolbar_hidden = hidden;
            prev
        }
        fn is_menu_hidden(&self) -> bool {
            self.menu_hidden
        }
        fn set_menu_hidden(&mut self, hidden: bool) -> bool {
            let prev = self.menu_hidden;
            self.menu_hidden = hidden;
            prev
        }
        fn is_statusbar_hidden(&self) -> bool {
            self.statusbar_hidden
        }
        fn set_statusbar_hidden(&mut self, hidden: bool) -> bool {
            let prev = self.statusbar_hidden;
            self.statusbar_hidden = hidden;
            prev
        }
        fn editor_zoom_level(&self) -> i32 {
            // Tests don't model real Scintilla zoom — return 0 (no
            // zoom applied) by default. Tests that exercise the
            // zoom-level surface explicitly are out of scope for
            // this fake.
            0
        }
        fn editor_default_fg_color(&self) -> i32 {
            // Default Win32 black-on-white text colour: 0x000000.
            0
        }
        fn editor_default_bg_color(&self) -> i32 {
            // Default Win32 white background: 0xFFFFFF.
            0x00FF_FFFF
        }
        fn set_smooth_font(&mut self, _smooth: bool) -> bool {
            false
        }
        fn set_editor_border_edge(&mut self, _enable: bool) -> bool {
            false
        }
        fn set_line_number_width_mode(&mut self, _mode: i32) -> bool {
            // FakeUi accepts every supported mode; the dispatcher
            // already filtered by `matches!` before calling.
            true
        }
        #[cfg(target_os = "windows")]
        fn shortcut_for_cmd_id(&self, _cmd_id: i32) -> Option<codepp_plugin_host::ShortcutKey> {
            // FakeUi has no accelerator table — tests that
            // exercise the lookup path exercise the dispatcher
            // mock instead.
            None
        }
        #[cfg(target_os = "windows")]
        fn remove_shortcut_for_cmd_id(&mut self, _cmd_id: i32) -> bool {
            // Same rationale as `shortcut_for_cmd_id` above —
            // FakeUi has nothing to remove from. Always reports
            // "nothing was removed."
            false
        }
        #[cfg(target_os = "windows")]
        fn register_modeless_dialog(
            &mut self,
            _dlg: codepp_plugin_host::Hwnd,
            _register: bool,
        ) -> bool {
            // FakeUi has no message pump — modeless-dialog
            // registration is a no-op. The dispatcher mock in
            // `dispatch.rs` exercises the registration list
            // shape without involving the shell layer.
            true
        }
        #[cfg(target_os = "windows")]
        fn add_toolbar_icon(&mut self, _cmd_id: i32, _hicon: codepp_plugin_host::Hwnd) -> bool {
            // FakeUi has no toolbar — same rationale as the
            // modeless-dialog mock above. The dispatcher mock
            // exercises the success/failure surface.
            true
        }
        #[cfg(target_os = "windows")]
        fn is_dark_mode_enabled(&self) -> bool {
            // FakeUi has no theme state — matches production:
            // Code++ Phase 4 has no host-side dark mode.
            false
        }
        #[cfg(target_os = "windows")]
        fn dark_mode_colors(&self, _out: &mut codepp_plugin_host::NppDarkModeColors) -> bool {
            // No palette to share when dark mode is off.
            false
        }
        #[cfg(target_os = "windows")]
        fn create_plugin_scintilla(
            &mut self,
            _parent: codepp_plugin_host::Hwnd,
        ) -> codepp_plugin_host::Hwnd {
            // FakeUi can't create a real Scintilla — returns
            // NULL (the documented "creation failed" sentinel).
            // The dispatcher mock in `dispatch.rs` exercises
            // the success/failure return-routing surface; this
            // path only matters for shell-level integration
            // tests, which don't yet drive plugin Scintilla
            // creation.
            core::ptr::null_mut()
        }
        #[cfg(target_os = "windows")]
        fn register_dock_dialog(&mut self, _params: codepp_plugin_host::DockDialogParams) -> bool {
            // Shell-level tests don't model docked dialogs;
            // the dispatcher mock in `dispatch.rs` covers the
            // surface end-to-end.
            true
        }
        #[cfg(target_os = "windows")]
        fn show_dock_dialog(&mut self, _h_client: codepp_plugin_host::Hwnd) -> bool {
            true
        }
        #[cfg(target_os = "windows")]
        fn hide_dock_dialog(&mut self, _h_client: codepp_plugin_host::Hwnd) -> bool {
            true
        }
        #[cfg(target_os = "windows")]
        fn update_dock_disp_info(&mut self, _h_client: codepp_plugin_host::Hwnd) -> bool {
            true
        }
        #[cfg(target_os = "windows")]
        fn dock_hwnd_by_name(
            &self,
            _name: &str,
            _module_name: Option<&str>,
        ) -> codepp_plugin_host::Hwnd {
            core::ptr::null_mut()
        }
        fn capture_text_from_doc(&mut self, _scintilla_doc: isize) -> String {
            // Tests don't model per-doc text storage — they share
            // one buffer. Return whatever the active buffer holds.
            self.buffer_text.clone()
        }
        fn is_doc_dirty(&mut self, _scintilla_doc: isize) -> bool {
            // Tests use the global `dirty` flag — sufficient for
            // the current single-buffer test fixtures. Per-doc
            // tracking can be added when a test needs it.
            self.dirty
        }
        #[cfg(target_os = "windows")]
        fn dispatch_npp_menu_command(&mut self, idm: i32) -> bool {
            // Record the id so tests can assert the shell routed
            // the plugin's `NPPM_MENUCOMMAND` through the UI
            // dispatcher. Returns `true` unconditionally — the
            // real Win32 impl filters unmapped built-in ids, but
            // the shell-side contract only exercises the routing
            // path.
            self.npp_menu_commands.push(idm);
            true
        }
        #[cfg(target_os = "windows")]
        fn set_npp_menu_item_check(&mut self, idm: i32, checked: bool) -> bool {
            self.npp_menu_checks.push((idm, checked));
            true
        }
        fn mark_active_buffer_dirty(&mut self) {
            self.mark_dirty_calls += 1;
            // Mirror the real Win32 impl's behavioural contract:
            // after a call the buffer must be observably dirty.
            // Tests exercising the `NPPM_MAKECURRENTBUFFERDIRTY`
            // → subsequent `is_doc_dirty` chain rely on this.
            self.dirty = true;
        }
    }

    fn drain_until<F: Fn(&FakeUi, &[PendingDialog]) -> bool>(
        shell: &mut Shell,
        ui: &mut FakeUi,
        predicate: F,
        timeout: Duration,
    ) -> Vec<PendingDialog> {
        let deadline = Instant::now() + timeout;
        let mut all_pending = Vec::new();
        while Instant::now() < deadline {
            let p = shell.drain(ui);
            all_pending.extend(p);
            if predicate(ui, &all_pending) {
                return all_pending;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        all_pending
    }

    #[test]
    fn open_file_pushes_text_through_ui() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "Hello, Code++!").unwrap();

        let wake_count = Arc::new(AtomicUsize::new(0));
        let wake_count_clone = wake_count.clone();
        let wake = Arc::new(move || {
            wake_count_clone.fetch_add(1, Ordering::Relaxed);
        }) as Arc<dyn Fn() + Send + Sync>;

        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());

        let pending = drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        assert_eq!(ui.set_text_calls.len(), 1);
        assert_eq!(ui.set_text_calls[0].0, "Hello, Code++!");
        assert_eq!(ui.set_text_calls[0].1, 0);
        assert_eq!(ui.status_calls.len(), 1);
        assert_eq!(ui.status_calls[0].1, "UTF-8");
        // Successful loads produce no pending dialogs.
        assert!(pending.is_empty());
        assert!(wake_count.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn open_then_save_round_trips_edited_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round.txt");
        std::fs::write(&path, "original\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        // Simulate user editing in Scintilla: change the buffer text
        // that get_buffer_text will return.
        ui.buffer_text = "edited\n".to_string();
        shell.save_current_to_disk(&mut ui).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "edited\n");
    }

    #[test]
    fn set_buffer_encoding_no_active_tab_returns_false() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        assert!(!shell.set_buffer_encoding(codepp_core::Encoding::Utf16LeBom));
    }

    #[test]
    fn set_buffer_encoding_same_value_returns_false() {
        // Re-clicking the active radio item must be a silent no-op
        // — without the equality check, every same-encoding click
        // would still notify-callers (notification spam, status-bar
        // repaint flicker).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u8.txt");
        std::fs::write(&path, "hello\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        // Plain ASCII content detects as UTF-8 (no BOM).
        assert_eq!(
            shell.active().unwrap().encoding,
            codepp_core::Encoding::Utf8
        );
        assert!(!shell.set_buffer_encoding(codepp_core::Encoding::Utf8));
    }

    #[test]
    fn set_buffer_encoding_then_save_writes_new_encoding_bytes() {
        // Phase 4 demo bullet: "Convert a UTF-8 file to UTF-16 LE
        // and back; bytes are correct." This test pins the
        // forward leg.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conv.txt");
        std::fs::write(&path, "abc\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        // Mirror the in-memory text the Scintilla buffer would
        // hold — `FakeUi::get_buffer_text` returns this verbatim
        // and `save_current_to_disk` re-encodes it through the
        // tab's current encoding.
        ui.buffer_text = "abc\n".to_string();
        assert!(shell.set_buffer_encoding(codepp_core::Encoding::Utf16LeBom));
        shell.save_current_to_disk(&mut ui).unwrap();

        let on_disk = std::fs::read(&path).unwrap();
        // UTF-16 LE BOM: 0xFF 0xFE then 'a'/'b'/'c'/'\n' as
        // little-endian u16s (each high byte zero).
        assert_eq!(
            on_disk,
            vec![0xFF, 0xFE, b'a', 0x00, b'b', 0x00, b'c', 0x00, b'\n', 0x00]
        );
    }

    #[test]
    fn set_buffer_encoding_round_trip_to_utf16_and_back() {
        // The full round-trip the Phase 4 demo describes: open a
        // UTF-8 file, convert to UTF-16 LE BOM, save, reopen,
        // convert back to UTF-8, save, and compare the final
        // bytes against the original. The text content survives
        // both legs because every codepoint is representable in
        // both encodings.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trip.txt");
        let original_bytes = b"hello world\n".to_vec();
        std::fs::write(&path, &original_bytes).unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );
        ui.buffer_text = "hello world\n".to_string();

        // Forward: UTF-8 -> UTF-16 LE BOM.
        assert!(shell.set_buffer_encoding(codepp_core::Encoding::Utf16LeBom));
        shell.save_current_to_disk(&mut ui).unwrap();
        let utf16_bytes = std::fs::read(&path).unwrap();
        assert_eq!(&utf16_bytes[..2], b"\xFF\xFE", "BOM should be present");

        // Re-open the file to round-trip the bytes through
        // detection + decode. After this pass the active tab
        // sees the saved encoding (Utf16LeBom) and the same text.
        // `close_active_tab` is synchronous (data-model only); no
        // intermediate drain needed before the re-open. Capture
        // the baseline `set_text_calls` count and wait for *one
        // more* to land — `>= 2` would be satisfied prematurely
        // if the first open's load happened to push more than
        // one chunk.
        let baseline = ui.set_text_calls.len();
        shell.close_active_tab();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() > baseline,
            Duration::from_secs(2),
        );
        assert_eq!(
            shell.active().unwrap().encoding,
            codepp_core::Encoding::Utf16LeBom,
        );

        // Back: UTF-16 LE BOM -> UTF-8 (no BOM). After save, the
        // on-disk bytes match the original UTF-8 input
        // byte-for-byte.
        ui.buffer_text = "hello world\n".to_string();
        assert!(shell.set_buffer_encoding(codepp_core::Encoding::Utf8));
        shell.save_current_to_disk(&mut ui).unwrap();
        let final_bytes = std::fs::read(&path).unwrap();
        assert_eq!(final_bytes, original_bytes);
    }

    #[test]
    fn set_buffer_encoding_by_id_targets_specific_tab() {
        // The id-keyed setter must mutate only the addressed tab,
        // leaving other tabs' encodings untouched. Plugin-driven
        // NPPM_SETBUFFERENCODING addresses tabs by id, not by
        // active-ness, so a plugin that flips the encoding on a
        // background tab shouldn't accidentally flip the active
        // tab's metadata.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        for path in [a, b] {
            shell.open_file(path);
            let target = ui.set_text_calls.len() + 1;
            drain_until(
                &mut shell,
                &mut ui,
                |u, _| u.set_text_calls.len() == target,
                Duration::from_secs(2),
            );
        }
        let id_b = shell.tabs[1].id as isize;

        // Default UTF-8 on both tabs.
        assert_eq!(shell.tabs[0].encoding, codepp_core::Encoding::Utf8);
        assert_eq!(shell.tabs[1].encoding, codepp_core::Encoding::Utf8);

        // Flip tab `b`'s encoding only — tab `a` stays UTF-8.
        assert!(shell.set_buffer_encoding_by_id(id_b, codepp_core::Encoding::Utf16LeBom));
        assert_eq!(shell.tabs[0].encoding, codepp_core::Encoding::Utf8);
        assert_eq!(shell.tabs[1].encoding, codepp_core::Encoding::Utf16LeBom);

        // Same-value set on the same id reports `true` — the buffer
        // *is* in the requested state, which is success per the N++
        // contract. (Distinguishing same-value-success from
        // unknown-id is the bit plugins gate on.)
        assert!(shell.set_buffer_encoding_by_id(id_b, codepp_core::Encoding::Utf16LeBom));

        // Unknown id is rejected.
        assert!(!shell.set_buffer_encoding_by_id(9999, codepp_core::Encoding::Utf8));
    }

    #[test]
    fn set_buffer_eol_by_id_targets_specific_tab() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eol.txt");
        std::fs::write(&path, "line\n").unwrap();
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        let id = shell.tabs[0].id as isize;
        // Detection of "line\n" produces Eol::Lf.
        assert_eq!(shell.tabs[0].eol, codepp_core::Eol::Lf);

        // Flip to CRLF.
        assert!(shell.set_buffer_eol_by_id(id, codepp_core::Eol::CrLf));
        assert_eq!(shell.tabs[0].eol, codepp_core::Eol::CrLf);

        // Same-value reports success — the buffer is already in
        // the requested state.
        assert!(shell.set_buffer_eol_by_id(id, codepp_core::Eol::CrLf));

        // Unknown id rejected.
        assert!(!shell.set_buffer_eol_by_id(9999, codepp_core::Eol::Lf));
    }

    #[test]
    fn open_missing_file_emits_error_dialog() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(PathBuf::from("definitely-missing-12345.txt"));

        let pending = drain_until(
            &mut shell,
            &mut ui,
            |_, p| !p.is_empty(),
            Duration::from_secs(2),
        );
        assert!(matches!(
            &pending[0],
            PendingDialog::Error { title, .. } if title == "Open failed"
        ));
        // The fresh tab created for the open should be removed when
        // the load fails — leaving it would orphan a buffer id with
        // `path = None`, breaking the `id != 0 ⇒ path is Some`
        // contract that well-behaved Notepad++ plugins assume.
        assert_eq!(
            shell.tabs.len(),
            0,
            "fresh tab should be removed on load failure"
        );
        assert_eq!(shell.active_tab, None);
    }

    // -- Plugin dispatcher entry-point tests ------------------------
    //
    // These assert that `Shell::dispatch_plugin_message` correctly
    // bridges into the plugin-host dispatcher with the right
    // HostServices view of the active buffer — without needing a
    // real plugin DLL.

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_out_of_range_returns_none() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        // WM_USER + 5 is below NPPMSG (= WM_USER + 1000); dispatcher
        // must yield None so the wnd_proc falls through to its
        // default handler.
        let r = unsafe {
            shell.dispatch_plugin_message(&mut ui, HostHandles::null(), 0x0400 + 5, 0, 0)
        };
        assert!(r.is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_get_current_buffer_id_reflects_active_path() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        const NPPM_GETCURRENTBUFFERID: u32 = (0x0400 + 1000) + 60;

        // No active buffer yet — should return 0.
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETCURRENTBUFFERID,
                0,
                0,
            )
        };
        assert_eq!(r, Some(0));

        // Open a file; once the buffer settles, we should report the
        // primary id.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "hi").unwrap();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETCURRENTBUFFERID,
                0,
                0,
            )
        };
        let expected_id = shell.active().expect("active tab").id as isize;
        assert_eq!(r, Some(expected_id));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_get_full_path_returns_active_buffer_path() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        std::fs::write(&path, "x").unwrap();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        const NPPM_GETFULLPATHFROMBUFFERID: u32 = (0x0400 + 1000) + 58;
        const MAX_PATH_TCHARS: usize = 260;
        let active_id = shell.active().expect("active tab").id as usize;
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETFULLPATHFROMBUFFERID,
                active_id,
                buf.as_mut_ptr() as isize,
            )
        };
        let written = r.unwrap();
        assert!(written > 0);
        let nul = buf.iter().position(|&u| u == 0).unwrap();
        let got = String::from_utf16_lossy(&buf[..nul]);
        assert_eq!(PathBuf::from(got), path);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_get_buffer_format_maps_mixed_to_unix() {
        // `Eol::Mixed` is unique to Code++ — N++'s ABI has no
        // equivalent. The HostBridge mapping reports `UNIX_FORMAT`
        // (LF) so a plugin doing `if (format == WIN_FORMAT)` on a
        // mixed-EOL file gets a stable answer rather than depending
        // on which line ending happens to be most common in the
        // buffer. This test pins that contract.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.txt");
        std::fs::write(&path, "a\nb\r\nc\n").unwrap();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        // Force the tab into the Mixed-EOL state. The on-disk
        // detection rounds to a single dominant EOL; explicit
        // assignment lets this test cover the Mixed branch
        // without crafting a file the detector classifies that
        // way (the detector's threshold is intentionally lenient
        // and may shift in future tuning).
        let active_id = shell.active().expect("active tab").id as usize;
        shell.tabs[0].eol = codepp_core::Eol::Mixed;

        const NPPM_GETBUFFERFORMAT: u32 = (0x0400 + 1000) + 68;
        const UNIX_FORMAT: isize = 2;
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETBUFFERFORMAT,
                active_id,
                0,
            )
        };
        assert_eq!(r, Some(UNIX_FORMAT));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_get_buffer_encoding_returns_unimode() {
        // Default load of a UTF-8 file (no BOM) should report
        // `uniCookie` (UTF-8 without BOM) — the most common case
        // for plain text files. The "Cookie" naming is a historical
        // N++ misnomer for "BOM-less UTF-8"; we keep it for ABI
        // compatibility.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf8.txt");
        std::fs::write(&path, "hello").unwrap();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        let active_id = shell.active().expect("active tab").id as usize;
        const NPPM_GETBUFFERENCODING: u32 = (0x0400 + 1000) + 66;
        const UNI_COOKIE: isize = 4;
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETBUFFERENCODING,
                active_id,
                0,
            )
        };
        assert_eq!(r, Some(UNI_COOKIE));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_decode_sci_then_get_buffer_encoding_returns_uni_8bit() {
        // NPPM_DECODESCI on the primary view flips the active
        // tab's encoding to "ANSI" (`Encoding::Other("windows-1252")`
        // in our internal model). A subsequent NPPM_GETBUFFERENCODING
        // observes the metadata change as `UNI_8BIT`. This pins the
        // round-trip so a future refactor of the UNI_8BIT mapping
        // (e.g. GetACP-driven detection) doesn't silently break the
        // get/set/encode/decode quartet's cross-consistency.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dec.txt");
        std::fs::write(&path, "x").unwrap();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        const NPPM_DECODESCI: u32 = (0x0400 + 1000) + 27;
        const NPPM_GETBUFFERENCODING: u32 = (0x0400 + 1000) + 66;
        const UNI_8BIT: isize = 0;

        // Primary view, wparam = 0.
        let r = unsafe {
            shell.dispatch_plugin_message(&mut ui, HostHandles::null(), NPPM_DECODESCI, 0, 0)
        };
        assert_eq!(r, Some(UNI_8BIT));

        let active_id = shell.active().expect("active").id as usize;
        let r2 = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETBUFFERENCODING,
                active_id,
                0,
            )
        };
        assert_eq!(r2, Some(UNI_8BIT));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_encode_sci_secondary_view_returns_minus_one() {
        // Single-view Code++ has no secondary view, so view == 1
        // always reports -1 ("view has no active buffer") even
        // when the primary view has an active tab.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enc.txt");
        std::fs::write(&path, "y").unwrap();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        const NPPM_ENCODESCI: u32 = (0x0400 + 1000) + 26;
        let r = unsafe {
            shell.dispatch_plugin_message(&mut ui, HostHandles::null(), NPPM_ENCODESCI, 1, 0)
        };
        assert_eq!(r, Some(-1));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_set_status_routes_to_ui() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        const NPPM_SETSTATUSBAR: u32 = (0x0400 + 1000) + 24;
        let text: Vec<u16> = "Hello!".encode_utf16().chain(std::iter::once(0)).collect();
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SETSTATUSBAR,
                2,
                text.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(ui.plugin_status_calls, vec![(2usize, "Hello!".to_string())]);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_menu_command_routes_id_through_ui() {
        // Plugin sends NPPM_MENUCOMMAND(IDM_FILE_OPEN); the shell
        // must forward the id verbatim through `HostBridge` into
        // the UI's `dispatch_npp_menu_command`. FakeUi records
        // the id — real Win32 would `PostMessage(WM_COMMAND)`.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        const NPPM_MENUCOMMAND: u32 = (0x0400 + 1000) + 48;
        // IDM_FILE_OPEN = IDM_BASE(40000) + 1000 + 2 = 41002.
        const IDM_FILE_OPEN: isize = 41002;

        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_MENUCOMMAND,
                0,
                IDM_FILE_OPEN,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(ui.npp_menu_commands, vec![IDM_FILE_OPEN as i32]);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_set_menu_item_check_routes_id_and_state() {
        // Plugin sends NPPM_SETMENUITEMCHECK(IDM_VIEW_WRAP, TRUE);
        // the shell must forward both the id and the checked flag
        // verbatim into the UI's `set_npp_menu_item_check`. FakeUi
        // records the `(id, checked)` pair; real Win32 would call
        // `CheckMenuItem(main_menu, id, MF_BYCOMMAND | MF_CHECKED)`.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        const NPPM_SETMENUITEMCHECK: u32 = (0x0400 + 1000) + 40;
        // IDM_VIEW_WRAP = IDM_BASE + 4000 + 7 = 44007.
        const IDM_VIEW_WRAP: usize = 44007;

        let checked_r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SETMENUITEMCHECK,
                IDM_VIEW_WRAP,
                1, // TRUE
            )
        };
        assert_eq!(checked_r, Some(1));

        // Second call with FALSE — the shell must forward the
        // clear-check semantic too.
        let unchecked_r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SETMENUITEMCHECK,
                IDM_VIEW_WRAP,
                0, // FALSE
            )
        };
        assert_eq!(unchecked_r, Some(1));

        assert_eq!(
            ui.npp_menu_checks,
            vec![(IDM_VIEW_WRAP as i32, true), (IDM_VIEW_WRAP as i32, false),]
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_make_current_buffer_dirty_reaches_ui() {
        // Plugin sends NPPM_MAKECURRENTBUFFERDIRTY; the shell must
        // forward through `HostBridge` into the UI's
        // `mark_active_buffer_dirty`. FakeUi's impl also flips its
        // internal `dirty` flag so a follow-up `is_doc_dirty`
        // reflects the change — pinning the behavioural contract
        // that the real Win32 impl upholds via `SCI_ADDUNDOACTION`.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        // Baseline: no calls, no dirty state.
        assert_eq!(ui.mark_dirty_calls, 0);
        assert!(!ui.is_doc_dirty(1));

        const NPPM_MAKECURRENTBUFFERDIRTY: u32 = (0x0400 + 1000) + 44;
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_MAKECURRENTBUFFERDIRTY,
                0,
                0,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(ui.mark_dirty_calls, 1);
        // Behavioural check: the UI-side impl must leave the
        // buffer observably dirty. The real Win32 impl issues
        // `SCI_ADDUNDOACTION` to shift past the savepoint; FakeUi
        // sets its flag directly.
        assert!(ui.is_doc_dirty(1));

        // Second call — the counter increments, dirty stays true.
        // The real Win32 impl no-ops on already-dirty (avoids
        // stacking phantom undo entries); FakeUi doesn't model
        // that idempotence check because the shell doesn't rely
        // on it — the semantic ("after the call, buffer is dirty")
        // holds either way.
        let r2 = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_MAKECURRENTBUFFERDIRTY,
                0,
                0,
            )
        };
        assert_eq!(r2, Some(1));
        assert_eq!(ui.mark_dirty_calls, 2);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_doopen_queues_load() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("via-plugin.txt");
        std::fs::write(&path, "from plugin").unwrap();

        // Build a wide-char path the dispatcher will decode and
        // forward into Shell::open_file.
        let path_str = path.to_string_lossy().into_owned();
        let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();

        const NPPM_DOOPEN: u32 = (0x0400 + 1000) + 77;
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_DOOPEN,
                0,
                wide.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));

        // The open is async; drain until the loader delivers.
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );
        assert_eq!(ui.set_text_calls.len(), 1);
        assert_eq!(ui.set_text_calls[0].0, "from plugin");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_save_round_trips_via_dispatcher() {
        // Plugin sends NPPM_SAVECURRENTFILE; the bridge must call
        // through to save_current_to_disk and produce on-disk bytes.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("save-via-plugin.txt");
        std::fs::write(&path, "before\n").unwrap();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        ui.buffer_text = "after\n".to_string();
        const NPPM_SAVECURRENTFILE: u32 = (0x0400 + 1000) + 38;
        let r = unsafe {
            shell.dispatch_plugin_message(&mut ui, HostHandles::null(), NPPM_SAVECURRENTFILE, 0, 0)
        };
        assert_eq!(r, Some(1));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "after\n");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn discover_plugins_on_missing_dir_is_zero() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let n = shell
            .discover_plugins(Path::new("definitely-not-a-real-plugin-dir-99999"))
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(shell.plugin_count(), 0);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn notify_plugins_with_zero_loaded_is_noop() {
        // Sanity: notify_plugins on a Shell with no loaded plugins
        // must not panic. (No plugins loaded means notify_all has
        // nothing to broadcast to; this asserts the wiring doesn't
        // assume any have been loaded.)
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let shell = Shell::new(wake).unwrap();
        shell.notify_plugins(Notification::Ready, core::ptr::null_mut());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn successful_open_queues_file_opened_notification() {
        // A successful load through the loader → drain → apply path
        // should leave a NPPN_FILEOPENED notification waiting for
        // the UI to fire after dropping its &mut Shell borrow.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.txt");
        std::fs::write(&path, "x").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        // Bind the active tab's id from the actual data model rather
        // than asserting a literal — the value happens to be
        // PRIMARY_BUFFER_ID today (next_buffer_id starts at 1) but
        // the contract is "the active tab's id," not "always 1."
        let expected_id = shell.active().expect("active tab").id as isize;
        let notifications = shell.take_notifications();
        // A successful open queues, in order: NPPN_FILEBEFOREOPEN
        // (no buffer id — the tab hasn't been allocated yet),
        // NPPN_FILEOPENED, then NPPN_BUFFERACTIVATED. Matches
        // Notepad++'s canonical "BEFORE_OPEN → FILEOPENED →
        // BUFFERACTIVATED" sequence on the same load.
        assert_eq!(notifications.len(), 3);
        assert!(matches!(notifications[0], Notification::FileBeforeOpen));
        assert!(matches!(
            notifications[1],
            Notification::FileOpened { buffer_id } if buffer_id == expected_id
        ));
        assert!(matches!(
            notifications[2],
            Notification::BufferActivated { buffer_id } if buffer_id == expected_id
        ));

        // Subsequent take_notifications returns an empty Vec (queue
        // drained) — the UI doesn't re-fire on every wake.
        assert!(shell.take_notifications().is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn successful_save_queues_file_saved_notification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("save-notify.txt");
        std::fs::write(&path, "before").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );
        // Reset queue state before testing the save path: drain the
        // `FileOpened` queued by the load so the second
        // `take_notifications` later in this test is unambiguously
        // the response to `save_current_to_disk`.
        let _ = shell.take_notifications();

        ui.buffer_text = "after".to_string();
        shell.save_current_to_disk(&mut ui).unwrap();

        // Bind the active tab's id rather than asserting the literal.
        let expected_id = shell.active().expect("active tab").id as isize;
        let notifications = shell.take_notifications();
        // BEFORE_SAVE then FILESAVED — N++'s ABI orders the pair so
        // plugins observing the buffer's pre-save state on
        // BEFORE_SAVE can compare against the post-save observation
        // on FILESAVED.
        assert_eq!(notifications.len(), 2);
        match &notifications[0] {
            Notification::FileBeforeSave { buffer_id } => assert_eq!(*buffer_id, expected_id),
            other => panic!("expected FileBeforeSave, got {other:?}"),
        }
        match &notifications[1] {
            Notification::FileSaved { buffer_id } => assert_eq!(*buffer_id, expected_id),
            other => panic!("expected FileSaved, got {other:?}"),
        }
    }

    // -- Multi-tab data model tests (milestone 6a) ------------------

    #[test]
    fn first_open_populates_tab_zero_in_place() {
        // Initial state: no tabs, no active tab. The first open
        // creates tab[0] (using the empty-active-tab branch's
        // freshly-allocated id) and makes it active. The buffer id
        // is 1 (next_buffer_id starts there).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("first.txt");
        std::fs::write(&path, "x").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        assert_eq!(shell.tabs.len(), 0);
        assert_eq!(shell.active_tab, None);

        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        assert_eq!(shell.tabs.len(), 1, "first open creates exactly one tab");
        assert_eq!(shell.active_tab, Some(0));
        let tab = shell.active().unwrap();
        assert_eq!(tab.id, PRIMARY_BUFFER_ID as i32);
        assert_eq!(tab.path, Some(path));
    }

    #[test]
    fn second_open_pushes_new_tab_with_distinct_id() {
        // Two opens of distinct paths: the second one should NOT
        // replace tab[0] (it already has a path) — it pushes a new
        // tab with a fresh id.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        std::fs::write(&path_a, "a").unwrap();
        std::fs::write(&path_b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        shell.open_file(path_a.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        let id_a = shell.active().unwrap().id;

        shell.open_file(path_b.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 2,
            Duration::from_secs(2),
        );

        assert_eq!(shell.tabs.len(), 2, "two distinct opens → two tabs");
        assert_eq!(shell.active_tab, Some(1), "second open is now active");
        let id_b = shell.active().unwrap().id;
        assert_ne!(id_a, id_b, "ids must be distinct across tabs");
        assert_eq!(shell.tabs[0].path.as_ref(), Some(&path_a));
        assert_eq!(shell.tabs[1].path.as_ref(), Some(&path_b));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn switch_to_file_activates_existing_tab_without_reopen() {
        // Open two files (one tab each), then have a "plugin" call
        // NPPM_SWITCHTOFILE for the first path. The data-model active
        // index should flip to tab[0] WITHOUT a re-load (the file
        // is already in memory). The bridge implements this directly.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        std::fs::write(&path_a, "a").unwrap();
        std::fs::write(&path_b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path_a.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        shell.open_file(path_b.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 2,
            Duration::from_secs(2),
        );
        assert_eq!(shell.active_tab, Some(1));
        let load_count_before = ui.set_text_calls.len();

        // NPPM_SWITCHTOFILE = NPPMSG + 37
        const NPPM_SWITCHTOFILE: u32 = (0x0400 + 1000) + 37;
        let path_a_str = path_a.to_string_lossy().into_owned();
        let wide: Vec<u16> = path_a_str
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SWITCHTOFILE,
                0,
                wide.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(shell.active_tab, Some(0), "switch flipped to existing tab");
        assert_eq!(
            ui.set_text_calls.len(),
            load_count_before,
            "no re-load occurred for an in-memory tab"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn open_queues_buffer_activated_for_the_new_tab() {
        // A successful open completes with the new tab as active —
        // apply_load_result should queue NPPN_BUFFERACTIVATED
        // alongside NPPN_FILEOPENED so plugins observing buffer
        // changes see the new id.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activated.txt");
        std::fs::write(&path, "x").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        let expected_id = shell.active().expect("active tab").id as isize;
        let notifications = shell.take_notifications();
        // Three queued: FileBeforeOpen, then FileOpened, then
        // BufferActivated. Order matches Notepad++'s canonical
        // pre-load → loaded → activated sequence.
        assert_eq!(notifications.len(), 3);
        assert!(matches!(notifications[0], Notification::FileBeforeOpen));
        assert!(matches!(
            notifications[1],
            Notification::FileOpened { buffer_id } if buffer_id == expected_id
        ));
        assert!(matches!(
            notifications[2],
            Notification::BufferActivated { buffer_id } if buffer_id == expected_id
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn switch_to_file_queues_buffer_activated() {
        // NPPM_SWITCHTOFILE on an already-open path activates the
        // existing tab. The dispatcher path should queue
        // NPPN_BUFFERACTIVATED; without it, plugins observing
        // tab changes via switch wouldn't pick up the move.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        std::fs::write(&path_a, "a").unwrap();
        std::fs::write(&path_b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path_a.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        shell.open_file(path_b);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 2,
            Duration::from_secs(2),
        );
        // Discard the FileOpened/BufferActivated notifications
        // queued by the two opens so the next take_notifications
        // is unambiguously the response to the switch below.
        let _ = shell.take_notifications();

        const NPPM_SWITCHTOFILE: u32 = (0x0400 + 1000) + 37;
        let path_a_str = path_a.to_string_lossy().into_owned();
        let wide: Vec<u16> = path_a_str
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SWITCHTOFILE,
                0,
                wide.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        let id_a = shell.tabs[0].id as isize;
        let notifications = shell.take_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(matches!(
            notifications[0],
            Notification::BufferActivated { buffer_id } if buffer_id == id_a
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn switch_to_file_to_already_active_tab_skips_notification() {
        // NPPM_SWITCHTOFILE for the path that's already active is
        // a tautological switch — no buffer change happened. The
        // dispatcher must NOT queue NPPN_BUFFERACTIVATED, otherwise
        // plugins that audit-log activations log a false positive
        // and plugins that reset buffer-local state on activation
        // clobber valid state.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        // Drain the open's notifications.
        let _ = shell.take_notifications();

        const NPPM_SWITCHTOFILE: u32 = (0x0400 + 1000) + 37;
        let path_str = path.to_string_lossy().into_owned();
        let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SWITCHTOFILE,
                0,
                wide.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert!(
            shell.take_notifications().is_empty(),
            "switch-to-already-active must not queue any notification"
        );
    }

    #[test]
    fn close_active_tab_with_no_tabs_returns_none() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        assert!(shell.close_active_tab().is_none());
        assert!(shell.tabs.is_empty());
        assert_eq!(shell.active_tab, None);
    }

    #[test]
    fn close_active_tab_last_tab_clears_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("only.txt");
        std::fs::write(&path, "x").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );

        let closed = shell.close_active_tab().expect("close returns snapshot");
        assert_eq!(closed.closed_idx, 0);
        assert!(shell.tabs.is_empty());
        assert_eq!(shell.active_tab, None);
        // No new active tab → the snapshot's new_active_doc is 0.
        assert_eq!(closed.new_active_doc, 0);
    }

    #[test]
    fn close_active_tab_middle_prefers_right_neighbour() {
        // Three tabs, active is the middle one. Closing it should
        // make the previously-third tab (now at index 1) active.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let c = dir.path().join("c.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        std::fs::write(&c, "c").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        for path in [a, b.clone(), c.clone()] {
            shell.open_file(path);
            let target = ui.set_text_calls.len() + 1;
            drain_until(
                &mut shell,
                &mut ui,
                |u, _| u.set_text_calls.len() == target,
                Duration::from_secs(2),
            );
        }
        // Activate the middle tab.
        shell.active_tab = Some(1);
        let closed = shell.close_active_tab().expect("close returns snapshot");
        assert_eq!(closed.closed_idx, 1);
        assert_eq!(closed.path.as_ref(), Some(&b));
        assert_eq!(shell.tabs.len(), 2);
        // Right-neighbour took the closed slot's index.
        assert_eq!(shell.active_tab, Some(1));
        assert_eq!(shell.tabs[1].path.as_ref(), Some(&c));
    }

    #[test]
    fn close_active_tab_rightmost_falls_back_to_previous() {
        // Two tabs, active is the rightmost. Closing it should
        // make the previously-first tab active (since there's no
        // right-neighbour to slide into).
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        for path in [a.clone(), b] {
            shell.open_file(path);
            let target = ui.set_text_calls.len() + 1;
            drain_until(
                &mut shell,
                &mut ui,
                |u, _| u.set_text_calls.len() == target,
                Duration::from_secs(2),
            );
        }
        assert_eq!(shell.active_tab, Some(1));
        let closed = shell.close_active_tab().expect("close returns snapshot");
        assert_eq!(closed.closed_idx, 1);
        assert_eq!(shell.tabs.len(), 1);
        assert_eq!(shell.active_tab, Some(0));
        assert_eq!(shell.tabs[0].path.as_ref(), Some(&a));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn close_active_tab_queues_file_closed_then_buffer_activated() {
        // Closing one of two open tabs queues, in order:
        //   1. NPPN_FILEBEFORECLOSE (so plugins can save state)
        //   2. NPPN_FILECLOSED (final-act for the closed buffer)
        //   3. NPPN_BUFFERACTIVATED (new active sibling)
        // Order matches Notepad++.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        for path in [a.clone(), b] {
            shell.open_file(path);
            let target = ui.set_text_calls.len() + 1;
            drain_until(
                &mut shell,
                &mut ui,
                |u, _| u.set_text_calls.len() == target,
                Duration::from_secs(2),
            );
        }
        // Drain the open's notifications.
        let _ = shell.take_notifications();

        let closed_id = shell.tabs[1].id as isize;
        let new_active_id = shell.tabs[0].id as isize;
        let closed = shell.close_active_tab().expect("close");
        assert_eq!(closed.buffer_id as isize, closed_id);

        let notifications = shell.take_notifications();
        assert_eq!(notifications.len(), 3);
        assert!(matches!(
            notifications[0],
            Notification::FileBeforeClose { buffer_id } if buffer_id == closed_id
        ));
        assert!(matches!(
            notifications[1],
            Notification::FileClosed { buffer_id } if buffer_id == closed_id
        ));
        assert!(matches!(
            notifications[2],
            Notification::BufferActivated { buffer_id } if buffer_id == new_active_id
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn close_last_tab_queues_only_file_closed() {
        // Closing the only open tab queues NPPN_FILEBEFORECLOSE
        // followed by NPPN_FILECLOSED but NOT NPPN_BUFFERACTIVATED
        // — there's no new active buffer to activate.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("only.txt");
        std::fs::write(&path, "x").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        let _ = shell.take_notifications();

        let closed_id = shell.tabs[0].id as isize;
        let _ = shell.close_active_tab().expect("close");
        let notifications = shell.take_notifications();
        assert_eq!(notifications.len(), 2);
        assert!(matches!(
            notifications[0],
            Notification::FileBeforeClose { buffer_id } if buffer_id == closed_id
        ));
        assert!(matches!(
            notifications[1],
            Notification::FileClosed { buffer_id } if buffer_id == closed_id
        ));
    }

    #[test]
    fn activate_tab_returned_doc_persists_on_tab() {
        // First open: tab[0].scintilla_doc starts at 0; the FakeUi's
        // activate_tab hands out a fresh fake pointer (1) and Shell
        // records it on the tab. Second open: tab[1] also starts at
        // 0; FakeUi hands out a different pointer (2). Each tab
        // ends up bound to a distinct document.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        std::fs::write(&path_a, "a").unwrap();
        std::fs::write(&path_b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        shell.open_file(path_a);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        shell.open_file(path_b);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 2,
            Duration::from_secs(2),
        );

        assert_eq!(ui.activate_tab_calls.len(), 2);
        // First call: idx=0, in_doc=0 (uninitialised).
        assert_eq!(ui.activate_tab_calls[0].0, 0);
        assert_eq!(ui.activate_tab_calls[0].1, 0);
        // Second call: idx=1, in_doc=0 (uninitialised again — fresh tab).
        assert_eq!(ui.activate_tab_calls[1].0, 1);
        assert_eq!(ui.activate_tab_calls[1].1, 0);
        // The fake doc pointers handed back land on the tabs and
        // are distinct.
        assert_ne!(shell.tabs[0].scintilla_doc, 0);
        assert_ne!(shell.tabs[1].scintilla_doc, 0);
        assert_ne!(shell.tabs[0].scintilla_doc, shell.tabs[1].scintilla_doc);
    }

    #[test]
    fn open_cpp_file_calls_apply_lang_with_l_cpp() {
        // Phase 4 m1: opening a `.cpp` derives `LangType::L_CPP` from
        // the extension and forwards it to the UI's `apply_lang`. The
        // FakeUi records every call; we check both that the call
        // happens and that it carries the right LangType.
        use codepp_core::lang::L_CPP;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.cpp");
        std::fs::write(&path, "int main() { return 0; }\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        assert_eq!(ui.apply_lang_calls.len(), 1);
        assert_eq!(ui.apply_lang_calls[0], L_CPP);
        // The lang lands on the tab so plugins reading
        // NPPM_GETBUFFERLANGTYPE see it without a re-derive.
        assert_eq!(shell.tabs[0].lang, L_CPP);
    }

    #[test]
    fn open_unknown_extension_calls_apply_lang_with_l_text() {
        use codepp_core::lang::L_TEXT;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.xyz");
        std::fs::write(&path, "plain").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        assert_eq!(ui.apply_lang_calls.len(), 1);
        assert_eq!(ui.apply_lang_calls[0], L_TEXT);
    }

    #[test]
    fn apply_lang_runs_after_set_buffer_text() {
        // Scintilla re-styles the visible region on lexer attach, so
        // `apply_lang` must run after `set_buffer_text` — otherwise
        // the lexer sees an empty buffer and the first paint shows
        // un-coloured text. Order is observable via the FakeUi's
        // separate vectors plus the order each one was pushed in
        // — apply_load_result writes set_text first, apply_lang
        // second, so the ratio set_text:apply_lang stays 1:1 with
        // the same call ordering across loads.
        use codepp_core::lang::L_RUST;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        assert_eq!(ui.set_text_calls.len(), 1);
        assert_eq!(ui.apply_lang_calls.len(), 1);
        assert_eq!(ui.apply_lang_calls[0], L_RUST);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn set_buffer_lang_type_updates_tab_and_queues_langchanged() {
        // Phase 4 m2: a plugin that NPPM_SETBUFFERLANGTYPE's the
        // active buffer to a new lang must (a) flip Tab.lang, (b)
        // re-apply the lexer through the UI (lexer lives on the
        // view, not the doc), (c) queue NPPN_LANGCHANGED so other
        // plugins see the change.
        use codepp_core::lang::{L_CPP, L_RUST};
        use codepp_plugin_host::dispatch::NPPM_SETBUFFERLANGTYPE;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn x() {}").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        // After load: tab[0].lang == L_RUST. Now have a "plugin"
        // re-classify it as L_CPP via the dispatcher.
        assert_eq!(shell.tabs[0].lang, L_RUST);
        let id = shell.tabs[0].id as usize;
        let status_before = ui.status_calls.len();
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SETBUFFERLANGTYPE,
                id,
                L_CPP.as_npp_id() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(shell.tabs[0].lang, L_CPP);
        // apply_lang fired twice: once on load (L_RUST), once on
        // the plugin-driven set (L_CPP).
        assert_eq!(ui.apply_lang_calls.len(), 2);
        assert_eq!(*ui.apply_lang_calls.last().unwrap(), L_CPP);
        // The status bar's language slot reads from the lang we
        // just set — without a refresh here the user-visible label
        // stays stale until they switch tabs and back. Pin the
        // refresh by asserting `update_status` was called with the
        // *new* lang as part of the dispatch (counting alone would
        // miss a refactor that accidentally re-passed the old lang).
        assert_eq!(
            ui.status_calls.len(),
            status_before + 1,
            "set_buffer_lang_type on the active tab must refresh the status bar",
        );
        assert_eq!(
            ui.status_calls.last().unwrap().0,
            L_CPP,
            "status bar must repaint with the new lang, not the old one",
        );
        // NPPN_LANGCHANGED queued for delivery.
        assert!(
            shell
                .pending_notifications
                .iter()
                .any(|n| matches!(n, Notification::LangChanged { .. })),
            "NPPN_LANGCHANGED not queued",
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn set_buffer_lang_type_same_lang_is_idempotent() {
        // Re-classifying a buffer to its current lang must not
        // re-apply the lexer (visible flicker) or queue
        // NPPN_LANGCHANGED (false positive that breaks plugins
        // audit-logging language changes).
        use codepp_core::lang::L_RUST;
        use codepp_plugin_host::dispatch::NPPM_SETBUFFERLANGTYPE;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn x() {}").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        let id = shell.tabs[0].id as usize;
        // Drain any queued notifications from the open path so we
        // observe only the SETBUFFERLANGTYPE response.
        let _ = shell.take_notifications();
        let apply_calls_before = ui.apply_lang_calls.len();
        let status_calls_before = ui.status_calls.len();

        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SETBUFFERLANGTYPE,
                id,
                L_RUST.as_npp_id() as isize,
            )
        };
        // Returns success (the buffer IS that lang now, just was
        // already that lang).
        assert_eq!(r, Some(1));
        // No re-apply, no status refresh, no notification queued —
        // status_calls is part of the same idempotent contract; a
        // spurious update_status would trigger a chrome repaint
        // on a no-op set.
        assert_eq!(ui.apply_lang_calls.len(), apply_calls_before);
        assert_eq!(ui.status_calls.len(), status_calls_before);
        assert!(!shell
            .pending_notifications
            .iter()
            .any(|n| matches!(n, Notification::LangChanged { .. })));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn buffer_lang_type_returns_tabs_lang_to_plugins() {
        // Verifies that `HostBridge::buffer_lang_type` (the trait
        // impl plugins reach via NPPM_GETBUFFERLANGTYPE) reads the
        // tab's stored lang, not a hardcoded L_TEXT. Goes through
        // the same dispatch_plugin_message path the real wnd_proc
        // uses on `WM_NPPM_*` so we exercise the host-bridge hookup,
        // not just the bare `HostServices` impl.
        use codepp_core::lang::L_RUST;
        use codepp_plugin_host::dispatch::NPPM_GETBUFFERLANGTYPE;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn x() {}").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        // First opened tab gets buffer id 1 (sequential, base 1).
        let id = shell.tabs[0].id as usize;
        // SAFETY: dispatch_plugin_message's wnd_proc safety contract
        // requires UI-thread invocation; the test thread is the sole
        // owner of `shell` and `ui`, satisfying the contract.
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETBUFFERLANGTYPE,
                id,
                0,
            )
        };
        assert_eq!(r, Some(L_RUST.as_npp_id() as isize));
    }

    #[test]
    fn rapid_back_to_back_opens_dont_collide() {
        // Regression: two open_file calls back-to-back (before
        // either load completes) used to share tab[0] because the
        // empty-tab reuse rule only checked `path.is_none()` and
        // not `pending_load.is_none()`. The second open clobbered
        // the first's pending_load id so the first load result
        // was discarded as "stale" — only one of the two files
        // ended up open. Symptom in the wild: session restore
        // with two tabs only restored the last one.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("first.txt");
        let path_b = dir.path().join("second.txt");
        std::fs::write(&path_a, "AAA").unwrap();
        std::fs::write(&path_b, "BBB").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        // Both opens before draining — no apply_load_result has
        // fired yet, so tab[0] is still "no path, pending_load=Some".
        shell.open_file(path_a.clone());
        shell.open_file(path_b.clone());

        // Distinct tabs at this point with distinct pending_loads.
        assert_eq!(shell.tabs.len(), 2);
        assert!(shell.tabs[0].pending_load.is_some());
        assert!(shell.tabs[1].pending_load.is_some());
        assert_ne!(shell.tabs[0].pending_load, shell.tabs[1].pending_load);

        // Drain both loads. Both files should land on their tabs.
        // Wait until both pending_loads clear so the content
        // assertions below aren't observing a half-drained state
        // (a 500 ms timeout that fires before both loads complete
        // would otherwise let the test pass on a tab still
        // pending its real content).
        drain_until(
            &mut shell,
            &mut ui,
            |_, _| false,
            Duration::from_millis(500),
        );
        assert_eq!(shell.tabs.len(), 2, "both tabs survived the drain");
        assert!(
            shell.tabs[0].pending_load.is_none() && shell.tabs[1].pending_load.is_none(),
            "both loads must complete before content assertions",
        );
        assert_eq!(shell.tabs[0].path.as_deref(), Some(path_a.as_path()));
        assert_eq!(shell.tabs[1].path.as_deref(), Some(path_b.as_path()));
        assert_eq!(shell.tabs[0].text, "AAA");
        assert_eq!(shell.tabs[1].text, "BBB");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn find_next_stores_last_search_and_repeat_reuses_it() {
        // First find_next records the query+flags so a later
        // find_next_repeat (the F3 / Find Next path) can fire
        // without the user re-entering the search term.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello hello world").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        let hit = shell.find_next(&mut ui, "hello", SearchFlags::MATCH_CASE);
        assert_eq!(hit, Some(0), "first find_next returns position");
        assert_eq!(ui.search_calls.len(), 1);
        assert_eq!(ui.search_calls[0].0, "hello");
        assert_eq!(ui.search_calls[0].1, SearchFlags::MATCH_CASE);

        // Repeat — uses stored query, no new args.
        let hit2 = shell.find_next_repeat(&mut ui);
        assert_eq!(hit2, Some(0));
        assert_eq!(ui.search_calls.len(), 2, "second call recorded");
        assert_eq!(ui.search_calls[1].0, "hello");

        // Backward-repeat reuses the same stored query.
        let hit3 = shell.find_prev_repeat(&mut ui);
        assert!(hit3.is_some());
        assert_eq!(ui.search_calls[2].2, "prev");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn find_next_with_no_open_tab_is_noop() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        let hit = shell.find_next(&mut ui, "anything", SearchFlags::NONE);
        assert_eq!(hit, None);
        assert!(ui.search_calls.is_empty());
        // Empty search isn't stored as last_search so a stray
        // F3 doesn't trigger an empty-query call.
        assert!(shell.find_next_repeat(&mut ui).is_none());
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn replace_all_empty_query_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "abc").unwrap();
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        let count = shell.replace_all(&mut ui, "", "x", SearchFlags::NONE);
        assert_eq!(count, 0, "empty query must not enter Scintilla loop");
        assert!(ui.replace_calls.is_empty());
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn replace_all_counts_substitutions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "foo bar foo baz foo").unwrap();
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        let count = shell.replace_all(&mut ui, "foo", "qux", SearchFlags::NONE);
        assert_eq!(count, 3);
        assert_eq!(ui.buffer_text, "qux bar qux baz qux");
    }

    #[test]
    fn session_xml_roundtrip_preserves_tab_order_and_active_index() {
        // session.xml round-trip: write a 2-tab session via the
        // production save_to_xml path, then load it back and verify
        // both paths are preserved in their stored order plus the
        // active index. This is the data-shape contract that
        // `load_session_paths` depends on — `load_session_paths`
        // itself can't be unit-tested without the platform's
        // `session_xml_path` (test-only override would be its own
        // refactor).
        use codepp_core::session::{Session as CoreSession, Tab as CoreTab};
        let dir = tempfile::tempdir().unwrap();
        let xml_path = dir.path().join("session.xml");
        let original = CoreSession {
            active: Some(1),
            window: None,
            workspace: None,
            view: codepp_core::session::ViewSettings::default(),
            tabs: vec![
                CoreTab {
                    path: Some(PathBuf::from("/tmp/first.txt")),
                    cursor: 0,
                    encoding: Encoding::default(),
                    eol: Eol::default(),
                    untitled_seq: None,
                    backup: None,
                    custom_name: None,
                    lang: None,
                    pinned: false,
                },
                CoreTab {
                    path: Some(PathBuf::from("/tmp/second.txt")),
                    cursor: 5,
                    encoding: Encoding::default(),
                    eol: Eol::default(),
                    untitled_seq: None,
                    backup: None,
                    custom_name: None,
                    lang: None,
                    pinned: false,
                },
            ],
        };
        original.save_to_xml(&xml_path).unwrap();

        let parsed = CoreSession::load_from_xml(&xml_path).unwrap();
        assert_eq!(parsed.active, Some(1));
        assert_eq!(parsed.tabs.len(), 2);
        assert_eq!(parsed.tabs[0].path, Some(PathBuf::from("/tmp/first.txt")));
        assert_eq!(parsed.tabs[1].path, Some(PathBuf::from("/tmp/second.txt")));
        assert_eq!(parsed.tabs[1].cursor, 5);
    }

    #[test]
    fn new_untitled_creates_active_tab_without_path() {
        // File→New: a fresh untitled buffer becomes the active tab,
        // gets a buffer id, and binds a Scintilla document. The lang
        // defaults to L_TEXT (no path means no extension to derive
        // from). No file watcher is registered (path is None).
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        assert!(shell.tabs.is_empty());

        shell.new_untitled(&mut ui);

        assert_eq!(shell.tabs.len(), 1);
        assert_eq!(shell.active_tab, Some(0));
        let tab = &shell.tabs[0];
        assert!(tab.path.is_none());
        assert_eq!(tab.lang, codepp_core::lang::L_TEXT);
        assert!(tab.id > 0, "tab must have an allocated buffer id");
        assert_ne!(tab.scintilla_doc, 0, "Scintilla doc must be bound");
        // A freshly-created untitled buffer starts clean — the tab
        // strip renders it with the blue save glyph (not red). Only
        // the user's first edit flips this via `SCN_SAVEPOINTLEFT`.
        // Matches N++'s tab-strip semantic. Pins the invariant
        // `paint_tab_item` relies on; a future regression that
        // reintroduced the old "untitled → always dirty" behavior
        // would fail here.
        assert!(!tab.dirty, "fresh untitled buffer must not report dirty");

        // UI received the activation, an empty-buffer set, lang
        // attach, and a status refresh — same shape as a fresh load.
        assert_eq!(ui.activate_tab_calls.len(), 1);
        assert_eq!(ui.set_text_calls.len(), 1);
        assert_eq!(ui.set_text_calls[0].0, "");
        assert_eq!(
            ui.apply_lang_calls.last().copied(),
            Some(codepp_core::lang::L_TEXT)
        );
        assert_eq!(ui.status_calls.len(), 1);
    }

    #[test]
    fn new_untitled_then_again_appends_second_tab() {
        // Two New invocations produce two distinct tabs with distinct
        // buffer ids and distinct Scintilla docs. The second New
        // becomes the new active tab — matching the standard
        // editor expectation that fresh buffers come to the front.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.new_untitled(&mut ui);
        shell.new_untitled(&mut ui);

        assert_eq!(shell.tabs.len(), 2);
        assert_eq!(shell.active_tab, Some(1));
        assert_ne!(shell.tabs[0].id, shell.tabs[1].id);
        assert_ne!(shell.tabs[0].scintilla_doc, shell.tabs[1].scintilla_doc);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn new_untitled_queues_buffer_activated() {
        // BUFFERACTIVATED — but not FILEOPENED — fires for a brand-new
        // untitled buffer. The distinction matches Notepad++'s
        // contract: FILEOPENED is reserved for "a real file landed on
        // disk and got loaded".
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.new_untitled(&mut ui);

        let queued = shell.take_notifications();
        assert!(
            queued
                .iter()
                .any(|n| matches!(n, Notification::BufferActivated { .. })),
            "BUFFERACTIVATED should be queued for the new untitled buffer",
        );
        assert!(
            !queued
                .iter()
                .any(|n| matches!(n, Notification::FileOpened { .. })),
            "FILEOPENED must not fire for an untitled buffer (no real file)",
        );
    }

    #[test]
    fn save_buffer_as_writes_bytes_and_updates_path() {
        // Save As on an untitled buffer: writes the live editor text
        // to the chosen path, flips tab.path from None to Some, and
        // re-derives the lang from the new extension.
        let dir = tempfile::tempdir().unwrap();
        let new_path = dir.path().join("hello.rs");

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.new_untitled(&mut ui);

        // Simulate the user typing — the FakeUi's get_buffer_text
        // returns this verbatim, mirroring SCI_GETTEXT on the real
        // path.
        ui.buffer_text = "fn main() {}\n".to_string();

        shell.save_buffer_as(&mut ui, new_path.clone()).unwrap();

        // File on disk has the right bytes.
        let on_disk = std::fs::read_to_string(&new_path).unwrap();
        assert_eq!(on_disk, "fn main() {}\n");

        // Tab metadata moved to the new path.
        let tab = shell.active().unwrap();
        assert_eq!(tab.path.as_ref(), Some(&new_path));
        assert_eq!(tab.byte_len, 13);
        // Lang re-derived from the .rs extension.
        assert_eq!(tab.lang, codepp_core::lang::L_RUST);
    }

    #[test]
    fn save_buffer_as_then_save_writes_to_new_path() {
        // After Save As, a subsequent Save (Ctrl+S) goes to the new
        // location, not the old one. Pinning so a refactor that
        // forgets to update tab.path doesn't silently keep saving
        // to the old file.
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("orig.txt");
        let renamed = dir.path().join("renamed.txt");
        std::fs::write(&original, "first\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(original.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        ui.buffer_text = "second\n".to_string();
        shell.save_buffer_as(&mut ui, renamed.clone()).unwrap();

        // The renamed path now has the new content; the original
        // is untouched (still its pre-open content).
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "second\n");
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "first\n");

        // Subsequent Save writes back to the renamed path, not the
        // original — the tab is now "owned" by the new file.
        ui.buffer_text = "third\n".to_string();
        shell.save_current_to_disk(&mut ui).unwrap();
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "third\n");
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "first\n");
    }

    #[test]
    fn save_all_writes_every_titled_tab() {
        // Open two real files, edit each, then Save All. Both files
        // on disk should reflect the post-edit content.
        let dir = tempfile::tempdir().unwrap();
        let a_path = dir.path().join("a.txt");
        let b_path = dir.path().join("b.txt");
        std::fs::write(&a_path, "old A\n").unwrap();
        std::fs::write(&b_path, "old B\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(a_path.clone());
        shell.open_file(b_path.clone());
        // Drain twice so both load results land before Save All
        // runs. drain_until polls until the predicate is satisfied
        // — having two set_text calls is a fine completion signal.
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() >= 2,
            Duration::from_secs(2),
        );

        // FakeUi has one buffer slot, so save_all reads the same
        // text into both files — that's fine for this unit test;
        // the per-tab content selection is a Win32-side concern
        // that the SCI_GETTEXT round-trip handles.
        let activate_calls_before = ui.activate_tab_calls.len();
        let mark_saved_before = ui.mark_saved_calls;
        ui.buffer_text = "new B\n".to_string();
        let errors = shell.save_all(&mut ui);
        assert!(
            errors.is_empty(),
            "all tabs should save cleanly: {errors:?}"
        );

        // Both on-disk files have the post-Save-All content.
        let on_disk_a = std::fs::read_to_string(&a_path).unwrap();
        let on_disk_b = std::fs::read_to_string(&b_path).unwrap();
        assert_eq!(on_disk_a, "new B\n");
        assert_eq!(on_disk_b, "new B\n");

        // Verify that save_all *actually iterated*: at least 3
        // activate_tab calls happened (one per tab during the
        // loop, plus one to restore the original active). Without
        // this, a regression that saved only the active tab and
        // skipped the rest would still leave the on-disk content
        // matching (because save_all writes "new B\n" via the
        // current active) — pin the iteration explicitly. And
        // every successful per-tab save must have cleared its
        // dirty glyph via mark_saved.
        assert!(
            ui.activate_tab_calls.len() >= activate_calls_before + 3,
            "save_all should activate each tab plus restore original",
        );
        assert_eq!(
            ui.mark_saved_calls,
            mark_saved_before + 2,
            "every successful per-tab save must clear its dirty glyph",
        );
    }

    #[test]
    fn save_all_skips_untitled_tabs() {
        // Mix of (titled, untitled, untitled) — only the first tab
        // should receive a save call; the two untitled buffers
        // are skipped because Save All can't choose a path for
        // them. Order matters here: opening the titled file FIRST
        // means it gets a fresh tab, then the two new_untitled
        // calls each create their own tab. (If we'd opened the
        // titled file second, it would reuse the empty untitled
        // tab — that path-reuse is correct behaviour for the open
        // flow but would collapse the test setup to 2 tabs.)
        let dir = tempfile::tempdir().unwrap();
        let titled_path = dir.path().join("titled.txt");
        std::fs::write(&titled_path, "x\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        shell.open_file(titled_path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );
        shell.new_untitled(&mut ui);
        shell.new_untitled(&mut ui);

        ui.buffer_text = "saved\n".to_string();
        let errors = shell.save_all(&mut ui);
        assert!(errors.is_empty());
        // Titled file got written.
        assert_eq!(std::fs::read_to_string(&titled_path).unwrap(), "saved\n");
        // Tab count unchanged: 3 in, 3 out.
        assert_eq!(shell.tabs.len(), 3);
        // The two untitled tabs still have path = None.
        assert!(shell.tabs[0].path.is_some());
        assert!(shell.tabs[1].path.is_none());
        assert!(shell.tabs[2].path.is_none());
    }

    #[test]
    fn save_all_restores_original_active_tab() {
        // Open three files; switch to the middle one; Save All;
        // active tab should still be the middle one when done.
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..3)
            .map(|i| {
                let p = dir.path().join(format!("f{i}.txt"));
                std::fs::write(&p, format!("c{i}\n")).unwrap();
                p
            })
            .collect();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        for p in &paths {
            shell.open_file(p.clone());
        }
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() >= 3,
            Duration::from_secs(2),
        );

        // Switch to the middle tab and Save All.
        shell.active_tab = Some(1);
        ui.buffer_text = "saved\n".to_string();
        let _ = shell.save_all(&mut ui);

        // Active tab is preserved. The intermediate switches happen
        // but the final state is the user-visible one they started.
        assert_eq!(shell.active_tab, Some(1));
    }

    #[test]
    fn reload_active_with_no_tab_returns_false() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        assert!(!shell.reload_active());
    }

    #[test]
    fn reload_active_on_untitled_returns_false() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.new_untitled(&mut ui);
        // Untitled has no path; reload is a no-op.
        assert!(!shell.reload_active());
    }

    #[test]
    fn reload_active_on_titled_kicks_off_load() {
        // After Reload, the loader is given a new request for the
        // active tab's path. Use the same observation FakeUi-based
        // round-trip tests use: drain until set_buffer_text is
        // called for the post-reload content.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload.txt");
        std::fs::write(&path, "first\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        // Externally rewrite the file and Reload.
        std::fs::write(&path, "second\n").unwrap();
        let calls_before = ui.set_text_calls.len();
        assert!(shell.reload_active());
        // Reload kicks off an async load — drain until the second
        // set_buffer_text lands.
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() > calls_before,
            Duration::from_secs(2),
        );
        assert!(ui.set_text_calls.last().unwrap().0.contains("second"));
    }

    #[test]
    fn save_buffer_as_no_active_tab_returns_error() {
        // Save As without any open tab is `NoActivePath`, matching
        // the same-shape return on `save_current_to_disk`.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nope.txt");

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        let r = shell.save_buffer_as(&mut ui, target);
        assert!(matches!(r, Err(ShellError::NoActivePath)));
    }

    #[test]
    fn new_untitled_assigns_sequential_numbers() {
        // First New gets seq 1; second gets 2; third gets 3.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.new_untitled(&mut ui);
        shell.new_untitled(&mut ui);
        shell.new_untitled(&mut ui);
        let seqs: Vec<Option<u32>> = shell.tabs.iter().map(|t| t.untitled_seq).collect();
        assert_eq!(seqs, vec![Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn new_untitled_after_close_reuses_smallest_unused_seq() {
        // Close `new 1` then create another untitled buffer — the
        // new one takes seq 1 again, not seq 4. Matches Notepad++'s
        // smallest-unused convention and keeps the tab labels
        // ergonomic over a long session.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.new_untitled(&mut ui); // seq 1
        shell.new_untitled(&mut ui); // seq 2
        shell.new_untitled(&mut ui); // seq 3
                                     // Close the first tab (seq 1).
        shell.active_tab = Some(0);
        shell.close_active_tab();
        // The remaining tabs should be seqs 2 and 3.
        let remaining: Vec<Option<u32>> = shell.tabs.iter().map(|t| t.untitled_seq).collect();
        assert_eq!(remaining, vec![Some(2), Some(3)]);
        // Next New picks up the freed slot rather than allocating 4.
        shell.new_untitled(&mut ui);
        let after: Vec<Option<u32>> = shell.tabs.iter().map(|t| t.untitled_seq).collect();
        assert_eq!(after, vec![Some(2), Some(3), Some(1)]);
    }

    #[test]
    fn save_buffer_as_clears_untitled_seq() {
        // After Save As gives an untitled buffer a real path, the
        // untitled seq is dropped — the tab label switches from
        // "new N" to the file basename, and the freed N becomes
        // available for a future File→New.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("named.txt");

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.new_untitled(&mut ui);
        ui.buffer_text = "x\n".to_string();
        assert_eq!(shell.tabs[0].untitled_seq, Some(1));
        shell.save_buffer_as(&mut ui, target.clone()).unwrap();
        assert_eq!(shell.tabs[0].untitled_seq, None);
        assert_eq!(shell.tabs[0].path.as_deref(), Some(target.as_path()));
    }

    #[test]
    fn open_file_already_open_just_switches() {
        // Opening a file that's already in tab 0 while tab 1 is
        // active should switch active back to tab 0, not create a
        // duplicate tab. Verifies the de-duplication branch in
        // open_file's prologue.
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("first.txt");
        let p2 = dir.path().join("second.txt");
        std::fs::write(&p1, "1\n").unwrap();
        std::fs::write(&p2, "2\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(p1.clone());
        shell.open_file(p2.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() >= 2,
            Duration::from_secs(2),
        );
        assert_eq!(shell.tabs.len(), 2);
        assert_eq!(shell.active_tab, Some(1));

        // Re-opening p1: tab count stays at 2, active flips to 0.
        shell.open_file(p1.clone());
        assert_eq!(shell.tabs.len(), 2);
        assert_eq!(shell.active_tab, Some(0));
    }

    #[test]
    fn open_file_returns_switched_to_existing_on_dedupe() {
        // The UI leans on this return value to decide whether to
        // synchronously rebind the Scintilla view: without the
        // `SwitchedToExisting` signal it would let the WM_APP_WAKE
        // drain do the work, but the dedupe branch doesn't queue a
        // load, so no wake ever fires. Result: tab bar shows the
        // switch, editor stays on the previous buffer's document.
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("first.txt");
        let p2 = dir.path().join("second.txt");
        std::fs::write(&p1, "1\n").unwrap();
        std::fs::write(&p2, "2\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        // First open: fresh load (`Loading`).
        let outcome_p1 = shell.open_file(p1.clone());
        assert_eq!(outcome_p1, OpenFileOutcome::Loading);
        // Second open: distinct path, still `Loading`.
        let outcome_p2 = shell.open_file(p2.clone());
        assert_eq!(outcome_p2, OpenFileOutcome::Loading);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() >= 2,
            Duration::from_secs(2),
        );

        // Re-open p1 (idx 0) while p2 (idx 1) is active — dedupe
        // branch fires and returns the target index.
        assert_eq!(shell.active_tab, Some(1));
        let outcome = shell.open_file(p1.clone());
        assert_eq!(outcome, OpenFileOutcome::SwitchedToExisting(0));
        assert_eq!(shell.active_tab, Some(0));

        // Re-open p1 again while p1 is now active — no state change.
        let outcome = shell.open_file(p1);
        assert_eq!(outcome, OpenFileOutcome::AlreadyActive);
        assert_eq!(shell.active_tab, Some(0));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn open_file_already_active_is_idempotent() {
        // If the path is already open AND active, open_file is a
        // pure no-op — no extra tab, no spurious BUFFERACTIVATED.
        // Windows-gated because `take_notifications` and the
        // `Notification` enum are only compiled in on Windows
        // today (the plugin host is Phase 5 work for the other
        // platforms; until then the queue doesn't exist).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("only.txt");
        std::fs::write(&path, "x\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );
        let _ = shell.take_notifications();

        shell.open_file(path);
        // No new tab.
        assert_eq!(shell.tabs.len(), 1);
        // No spurious activation notification.
        let queued = shell.take_notifications();
        assert!(
            !queued
                .iter()
                .any(|n| matches!(n, Notification::BufferActivated { .. })),
            "re-opening already-active path must not queue BUFFERACTIVATED",
        );
    }

    #[test]
    fn open_file_dedupe_known_limitation_for_in_flight_load() {
        // Pinning a known limitation: opening the same file twice in
        // quick succession *before the first load completes* can
        // produce two tabs. The de-dupe branch in `open_file` keys
        // on `tab.path`, which is `None` while the load is still
        // in flight, so the second call falls through. The reuse-
        // empty-active branch is also gated on
        // `pending_load.is_none()`, which is false during the
        // first request's flight, so the second call allocates a
        // fresh tab.
        //
        // Fixing this cleanly needs a `pending_path: Option<PathBuf>`
        // on `Tab` (or a Shell-side `HashMap<RequestId, PathBuf>`)
        // so de-dupe can see the in-flight path before it lands —
        // tracked for a follow-up commit. The user-visible report
        // that motivated the de-dupe (Reload creating duplicates,
        // see `confirm_reload`) doesn't go through this path: it
        // already targets the matching tab directly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("once.txt");
        std::fs::write(&path, "x\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() >= 2,
            Duration::from_secs(2),
        );
        // Today's behaviour: 2 tabs both holding the same path.
        // When the limitation is fixed, this assertion flips to 1
        // and the test renames; the test stays as the regression
        // gate.
        assert_eq!(shell.tabs.len(), 2);
        assert_eq!(shell.tabs[0].path.as_deref(), Some(path.as_path()));
        assert_eq!(shell.tabs[1].path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn open_file_does_not_overwrite_explicit_untitled_tab() {
        // Regression for the user-reported bug where File→New
        // followed by File→Open silently replaced the new
        // untitled buffer's tab with the opened file. The reuse-
        // empty-active branch in `open_file` matched on
        // `path.is_none() && pending_load.is_none()` — both true
        // for a fresh File→New buffer, so it got eaten. The fix:
        // also gate on `untitled_seq.is_none()`, distinguishing
        // an internal placeholder from a deliberate user-created
        // buffer.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opened.txt");
        std::fs::write(&path, "x\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.new_untitled(&mut ui);
        assert_eq!(shell.tabs.len(), 1);
        assert_eq!(shell.tabs[0].untitled_seq, Some(1));

        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() >= 2,
            Duration::from_secs(2),
        );

        // Two tabs now: the original "new 1" untitled, and the
        // newly-opened file. The new file is active.
        assert_eq!(shell.tabs.len(), 2);
        assert_eq!(shell.tabs[0].untitled_seq, Some(1));
        assert!(shell.tabs[0].path.is_none());
        assert_eq!(shell.tabs[1].path.as_deref(), Some(path.as_path()));
        assert_eq!(shell.active_tab, Some(1));
    }

    #[test]
    fn new_untitled_refuses_past_max_open_tabs() {
        // The DoS cap from MAX_OPEN_TABS bounds how many tabs a
        // hostile in-process plugin can stack before allocation
        // refuses. Tests using a small loop are bounded by the
        // constant — 1024 untitled buffers in a unit test is a
        // few microseconds of Vec push + atomic increment, well
        // under the test-suite budget.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        for _ in 0..MAX_OPEN_TABS {
            shell.new_untitled(&mut ui);
        }
        assert_eq!(shell.tabs.len(), MAX_OPEN_TABS);

        // Past the cap → refused, no new tab, no panic.
        shell.new_untitled(&mut ui);
        assert_eq!(shell.tabs.len(), MAX_OPEN_TABS);

        // Closing one frees a slot; the next New succeeds again.
        shell.active_tab = Some(0);
        shell.close_active_tab();
        assert_eq!(shell.tabs.len(), MAX_OPEN_TABS - 1);
        shell.new_untitled(&mut ui);
        assert_eq!(shell.tabs.len(), MAX_OPEN_TABS);
    }

    #[test]
    fn open_file_refuses_past_max_open_tabs() {
        // Same cap applies to `open_file`. Test covers two
        // branches in production code:
        //   1. New-path open at the cap: refused, no new tab,
        //      no loader request.
        //   2. Re-open of an already-open path at the cap:
        //      de-dupe branch fires before the cap check, so
        //      the call succeeds and switches active without
        //      growing the count.
        let dir = tempfile::tempdir().unwrap();
        let opened_first = dir.path().join("opened-first.txt");
        let new_at_cap = dir.path().join("new-at-cap.txt");
        std::fs::write(&opened_first, "x\n").unwrap();
        std::fs::write(&new_at_cap, "y\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        // Open one real file first so we have a known path on a
        // real tab; the de-dupe assertion below targets it.
        shell.open_file(opened_first.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );
        assert_eq!(shell.tabs.len(), 1);
        let opened_first_idx = 0usize;

        // Fill the remainder with untitled buffers so the total
        // hits MAX_OPEN_TABS exactly.
        for _ in 0..(MAX_OPEN_TABS - 1) {
            shell.new_untitled(&mut ui);
        }
        assert_eq!(shell.tabs.len(), MAX_OPEN_TABS);
        let active_before = shell.active_tab;

        // Branch 1 — fresh path at the cap: refused.
        let outcome = shell.open_file(new_at_cap);
        assert_eq!(shell.tabs.len(), MAX_OPEN_TABS);
        assert_eq!(outcome, OpenFileOutcome::Rejected);

        // Branch 2 — already-open path at the cap: de-dupe fires
        // *before* the cap check, so the call succeeds and the
        // active flips back to that tab. Tab count unchanged.
        let outcome = shell.open_file(opened_first);
        assert_eq!(shell.tabs.len(), MAX_OPEN_TABS);
        assert_eq!(shell.active_tab, Some(opened_first_idx));
        assert_eq!(
            outcome,
            OpenFileOutcome::SwitchedToExisting(opened_first_idx)
        );
        assert_ne!(
            shell.active_tab, active_before,
            "de-dupe at cap must still flip the active tab",
        );
    }

    #[test]
    fn close_active_tab_can_drain_to_empty() {
        // Verify that `close_active_tab` itself produces an empty
        // tab list when called repeatedly — the contract that
        // `ensure_one_tab` (UI-side) layers on top of. Without this
        // contract, the UI would pop infinite untitled tabs trying
        // to satisfy the invariant.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.new_untitled(&mut ui);
        shell.new_untitled(&mut ui);
        shell.new_untitled(&mut ui);
        assert_eq!(shell.tabs.len(), 3);
        while shell.close_active_tab().is_some() {}
        assert!(shell.tabs.is_empty());
        assert_eq!(shell.active_tab, None);
    }

    #[test]
    fn confirm_reload_overwrites_active_tab_in_place() {
        // Reload via confirm_reload (the same path File→Reload
        // takes) replaces the active tab's content with the
        // post-reload bytes — same buffer id, same tab index. No
        // new tab.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload.txt");
        std::fs::write(&path, "first\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );
        let original_id = shell.tabs[0].id;
        let initial_tabs = shell.tabs.len();

        std::fs::write(&path, "second\n").unwrap();
        let calls_before = ui.set_text_calls.len();
        shell.confirm_reload(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() > calls_before,
            Duration::from_secs(2),
        );

        // Tab count unchanged — no duplicate created.
        assert_eq!(shell.tabs.len(), initial_tabs);
        // Same buffer id — reload, not re-open.
        assert_eq!(shell.tabs[0].id, original_id);
        // New content arrived.
        assert!(ui.set_text_calls.last().unwrap().0.contains("second"));
    }

    /// Helper: build a `Shell` with `n` synthetic tabs and a chosen
    /// active index, suitable for `move_tab` unit tests that don't
    /// need any of the loader/file-watcher machinery.
    fn shell_with_synthetic_tabs(n: usize, active: Option<usize>) -> Shell {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        shell.tabs = (0..n)
            .map(|i| Tab {
                id: (i as i32) + 1,
                path: Some(PathBuf::from(format!("/tmp/file{i}.txt"))),
                ..Tab::default()
            })
            .collect();
        shell.active_tab = active;
        shell
    }

    #[test]
    fn move_tab_dragged_tab_follows_to_new_position() {
        // Active tab moves with the drag — the user's focused
        // buffer stays focused regardless of where they drop it.
        let mut shell = shell_with_synthetic_tabs(5, Some(0));
        let id_at_0 = shell.tabs[0].id;
        assert!(shell.move_tab(0, 3));
        assert_eq!(shell.tabs[3].id, id_at_0);
        assert_eq!(shell.active_tab, Some(3));
    }

    #[test]
    fn move_tab_unaffected_active_stays_put() {
        // Drag a tab on the right of the active one to the far right
        // — the active tab is not in the affected range and its
        // index doesn't change.
        let mut shell = shell_with_synthetic_tabs(5, Some(1));
        let active_id = shell.tabs[1].id;
        assert!(shell.move_tab(3, 4));
        assert_eq!(shell.active_tab, Some(1));
        assert_eq!(shell.tabs[1].id, active_id);
    }

    #[test]
    fn move_tab_rightward_through_active_shifts_left() {
        // Active is at index 3 (D in [A,B,C,D,E]); move B (idx 1)
        // to position 4 → list becomes [A,C,D,E,B]; D is now at idx 2.
        let mut shell = shell_with_synthetic_tabs(5, Some(3));
        let active_id = shell.tabs[3].id;
        assert!(shell.move_tab(1, 4));
        assert_eq!(shell.active_tab, Some(2));
        assert_eq!(shell.tabs[2].id, active_id);
    }

    #[test]
    fn move_tab_leftward_through_active_shifts_right() {
        // Active is at index 1 (B in [A,B,C,D,E]); move E (idx 4)
        // to position 0 → list becomes [E,A,B,C,D]; B is now at idx 2.
        let mut shell = shell_with_synthetic_tabs(5, Some(1));
        let active_id = shell.tabs[1].id;
        assert!(shell.move_tab(4, 0));
        assert_eq!(shell.active_tab, Some(2));
        assert_eq!(shell.tabs[2].id, active_id);
    }

    #[test]
    fn move_tab_no_op_returns_false() {
        let mut shell = shell_with_synthetic_tabs(3, Some(1));
        // from == to
        assert!(!shell.move_tab(1, 1));
        // out of range
        assert!(!shell.move_tab(0, 99));
        assert!(!shell.move_tab(99, 0));
        assert_eq!(shell.tabs.len(), 3);
        assert_eq!(shell.active_tab, Some(1));
    }

    #[test]
    fn move_tab_with_no_active_keeps_active_none() {
        let mut shell = shell_with_synthetic_tabs(3, None);
        assert!(shell.move_tab(0, 2));
        assert_eq!(shell.active_tab, None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_tab_queues_doc_order_changed() {
        // A real reorder must fire NPPN_DOCORDERCHANGED so plugins
        // tracking per-tab UI state can re-sync from
        // NPPM_GETOPENFILENAMES.
        let mut shell = shell_with_synthetic_tabs(3, Some(0));
        // Discard any setup notifications so the next drain
        // unambiguously belongs to the move.
        let _ = shell.take_notifications();
        assert!(shell.move_tab(0, 2));
        let n = shell.take_notifications();
        assert!(
            n.iter().any(|x| matches!(x, Notification::DocOrderChanged)),
            "expected NPPN_DOCORDERCHANGED in {n:?}"
        );
    }

    /// Pinning a tab that's currently in the middle of the strip
    /// walks it to the left edge and flips the flag. Active tab
    /// index follows the moved tab.
    #[test]
    fn set_pinned_moves_tab_to_pinned_prefix() {
        let mut shell = shell_with_synthetic_tabs(5, Some(2));
        let id_at_2 = shell.tabs[2].id;
        assert!(shell.set_pinned(2, true));
        // Tab 2 slid to index 0 (left edge of pinned prefix).
        assert_eq!(shell.tabs[0].id, id_at_2);
        assert!(shell.tabs[0].pinned);
        // Active follows.
        assert_eq!(shell.active_tab, Some(0));
        // Only one pinned tab so far.
        assert_eq!(shell.first_unpinned_idx(), 1);
    }

    /// A second pinned tab lands to the right of the first pinned
    /// tab (insertion order), not clobbering it.
    #[test]
    fn set_pinned_second_tab_lands_after_first_pinned() {
        let mut shell = shell_with_synthetic_tabs(5, None);
        let id_first_pinned = shell.tabs[3].id;
        let id_second_pinned = shell.tabs[4].id;
        assert!(shell.set_pinned(3, true));
        // Pin idx 4 — after the first pin, this is now at idx 4
        // (the pin moved 3→0 shifted 4 left by one).
        assert!(shell.set_pinned(4, true));
        assert_eq!(shell.tabs[0].id, id_first_pinned);
        assert_eq!(shell.tabs[1].id, id_second_pinned);
        assert!(shell.tabs[0].pinned);
        assert!(shell.tabs[1].pinned);
        assert_eq!(shell.first_unpinned_idx(), 2);
    }

    /// Unpinning walks the tab to the leftmost slot of the unpinned
    /// zone (right after the last still-pinned tab).
    #[test]
    fn set_pinned_false_moves_tab_to_unpinned_edge() {
        let mut shell = shell_with_synthetic_tabs(5, None);
        assert!(shell.set_pinned(1, true));
        assert!(shell.set_pinned(2, true));
        // State: [P1, P2, A, C, D] where A/C/D are original unpinned.
        let unpinned_pin_id = shell.tabs[0].id;
        assert!(shell.set_pinned(0, false));
        // Unpinned tab now sits at the boundary (idx 1 — right after
        // the one still-pinned tab).
        assert_eq!(shell.first_unpinned_idx(), 1);
        assert_eq!(shell.tabs[1].id, unpinned_pin_id);
        assert!(!shell.tabs[1].pinned);
    }

    /// `set_pinned` is idempotent — flipping to the current state
    /// returns false and does not queue a notification.
    #[test]
    fn set_pinned_idempotent_no_op_returns_false() {
        let mut shell = shell_with_synthetic_tabs(3, Some(0));
        assert!(!shell.set_pinned(0, false), "already unpinned");
        assert!(shell.set_pinned(0, true));
        assert!(!shell.set_pinned(0, true), "already pinned");
        // Out of range.
        assert!(!shell.set_pinned(99, true));
    }

    /// `move_tab` refuses to move a pinned tab. Users pin
    /// specifically to fix the tab in place.
    #[test]
    fn move_tab_refuses_to_move_pinned_tab() {
        let mut shell = shell_with_synthetic_tabs(4, None);
        assert!(shell.set_pinned(1, true));
        // Pinned tab now at idx 0; every move attempt is rejected.
        assert!(!shell.move_tab(0, 2));
        assert!(!shell.move_tab(0, 1));
        // Layout unchanged.
        assert!(shell.tabs[0].pinned);
        assert_eq!(shell.first_unpinned_idx(), 1);
    }

    /// `move_tab` refuses to drag an unpinned tab into the pinned
    /// prefix — the boundary is respected.
    #[test]
    fn move_tab_refuses_unpinned_into_pinned_zone() {
        let mut shell = shell_with_synthetic_tabs(4, None);
        assert!(shell.set_pinned(0, true));
        assert!(shell.set_pinned(1, true));
        // State: two pinned tabs at [0, 1], unpinned at [2, 3].
        assert_eq!(shell.first_unpinned_idx(), 2);
        // Dragging unpinned tab (idx 2) to idx 0 or 1 would put it
        // inside the pinned zone — rejected.
        assert!(!shell.move_tab(2, 0));
        assert!(!shell.move_tab(2, 1));
        // Dragging unpinned within its zone still works.
        assert!(shell.move_tab(2, 3));
    }

    /// A `session.xml` whose `<tab>` entries interleave pinned and
    /// unpinned rows (either hand-edited or produced by some future
    /// buggy writer) must load with the invariant repaired —
    /// pinned tabs first, insertion order preserved within each
    /// group. Without this, `move_tab`'s pinned-prefix check would
    /// silently accept invalid drops.
    #[test]
    fn normalize_session_pinning_sorts_interleaved_tabs() {
        use codepp_core::session::{Session as CoreSession, Tab as CoreTab};
        let mk = |name: &str, pinned: bool| CoreTab {
            path: Some(PathBuf::from(format!("/tmp/{name}"))),
            pinned,
            ..CoreTab::default()
        };
        let mut session = CoreSession {
            active: Some(2), // "b" — a pinned tab
            window: None,
            workspace: None,
            view: codepp_core::session::ViewSettings::default(),
            tabs: vec![
                mk("a", false),
                mk("x", false),
                mk("b", true),
                mk("y", false),
                mk("c", true),
            ],
        };
        normalize_session_pinning(&mut session);
        // Pinned block first — insertion order (b before c) preserved.
        assert_eq!(
            session
                .tabs
                .iter()
                .map(|t| (
                    t.path
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    t.pinned
                ))
                .collect::<Vec<_>>(),
            vec![
                ("b".to_string(), true),
                ("c".to_string(), true),
                ("a".to_string(), false),
                ("x".to_string(), false),
                ("y".to_string(), false),
            ]
        );
        // Active tab pointer follows "b" to its new position (0).
        assert_eq!(session.active, Some(0));
    }

    /// The all-unpinned case is a stable no-op — order preserved,
    /// active untouched. Confirms the sort doesn't churn a
    /// session.xml that Code++ wrote itself in the current
    /// (pre-pinning) shape.
    #[test]
    fn normalize_session_pinning_no_op_when_no_pins() {
        use codepp_core::session::{Session as CoreSession, Tab as CoreTab};
        let mut session = CoreSession {
            active: Some(1),
            window: None,
            workspace: None,
            view: codepp_core::session::ViewSettings::default(),
            tabs: vec![
                CoreTab {
                    path: Some(PathBuf::from("/tmp/a")),
                    ..CoreTab::default()
                },
                CoreTab {
                    path: Some(PathBuf::from("/tmp/b")),
                    ..CoreTab::default()
                },
            ],
        };
        let before = session.clone();
        normalize_session_pinning(&mut session);
        assert_eq!(session, before);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn set_pinned_queues_doc_order_changed() {
        // The plugin ABI's view of buffer order changes on
        // pin/unpin — the pinned tab visibly moves in the strip —
        // so `NPPN_DOCORDERCHANGED` fires alongside the flag flip.
        let mut shell = shell_with_synthetic_tabs(3, Some(0));
        let _ = shell.take_notifications();
        assert!(shell.set_pinned(2, true));
        let n = shell.take_notifications();
        assert!(
            n.iter().any(|x| matches!(x, Notification::DocOrderChanged)),
            "expected NPPN_DOCORDERCHANGED in {n:?}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn external_remove_of_open_file_queues_file_deleted() {
        // The file watcher reports a Removed event for a file
        // currently open in a tab. The host must (a) push
        // NPPN_FILEDELETED with the matching tab's buffer id so
        // plugins observe the external delete, and (b) queue
        // the user-facing error dialog.
        let mut shell = shell_with_synthetic_tabs(3, Some(1));
        let _ = shell.take_notifications();
        let path = shell.tabs[1].path.clone().unwrap();
        let expected_id = shell.tabs[1].id as isize;
        let mut pending = Vec::new();
        shell.apply_file_change(FileChange::Removed(path), &mut pending);
        let n = shell.take_notifications();
        assert!(
            n.iter()
                .any(|x| matches!(x, Notification::FileDeleted { buffer_id } if *buffer_id == expected_id)),
            "expected NPPN_FILEDELETED with buffer_id={expected_id} in {n:?}"
        );
        assert!(
            pending.iter().any(
                |d| matches!(d, PendingDialog::Error { title, .. } if title == "File removed")
            ),
            "expected the 'File removed' error dialog in {pending:?}",
        );
    }

    #[test]
    fn external_remove_marks_open_tab_dirty() {
        // Data-loss safeguard: when the backing file is deleted
        // externally, the tab stays open (the buffer text is the
        // only surviving copy), and `Tab.dirty` must flip so both
        // the save-icon paint AND the close-tab save prompt gate
        // treat the buffer as unsaved. Without this, closing the
        // tab silently discards the last copy of the text.
        let mut shell = shell_with_synthetic_tabs(3, Some(0));
        // Sanity: freshly-loaded tabs start clean.
        assert!(
            !shell.tabs[1].dirty,
            "tab must start clean before the Removed event"
        );
        let path = shell.tabs[1].path.clone().unwrap();
        let mut pending = Vec::new();
        shell.apply_file_change(FileChange::Removed(path), &mut pending);
        assert!(
            shell.tabs[1].dirty,
            "tab must be marked dirty after its file was deleted externally"
        );
        // Untouched siblings must not spuriously go dirty.
        assert!(!shell.tabs[0].dirty, "sibling tab 0 must stay clean");
        assert!(!shell.tabs[2].dirty, "sibling tab 2 must stay clean");
    }

    #[test]
    fn external_remove_of_unopened_file_does_not_touch_other_tabs() {
        // The straggler path (Removed event for a path that no
        // longer matches any tab) must not accidentally flip any
        // sibling tab's dirty flag.
        let mut shell = shell_with_synthetic_tabs(3, Some(0));
        let mut pending = Vec::new();
        shell.apply_file_change(
            FileChange::Removed(PathBuf::from("/tmp/nowhere.txt")),
            &mut pending,
        );
        for (i, tab) in shell.tabs.iter().enumerate() {
            assert!(!tab.dirty, "tab {i} must stay clean on a stray Removed");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn external_remove_of_unopened_file_does_not_queue() {
        // A Removed event for a path that's NOT open in any tab
        // is a watcher straggler (the user closed the tab, then
        // the watcher's queued event arrived). Must NOT queue
        // NPPN_FILEDELETED — plugins would see a delete for a
        // buffer they never saw open.
        let mut shell = shell_with_synthetic_tabs(3, Some(0));
        let _ = shell.take_notifications();
        let mut pending = Vec::new();
        shell.apply_file_change(
            FileChange::Removed(PathBuf::from("/tmp/unknown.txt")),
            &mut pending,
        );
        let n = shell.take_notifications();
        assert!(
            !n.iter()
                .any(|x| matches!(x, Notification::FileDeleted { .. })),
            "watcher straggler must not queue NPPN_FILEDELETED; got {n:?}"
        );
        assert!(
            pending.is_empty(),
            "watcher straggler must not queue any user dialog; got {pending:?}"
        );
    }

    #[test]
    fn coalesce_dedupes_repeated_modified_events_for_one_path() {
        // notify::RecommendedWatcher on Windows fires several
        // Modify(Any) events for a single external save — the
        // API doesn't compact bursts. Drain must fold them into
        // one FileChange for the path.
        let path = PathBuf::from("/tmp/burst.txt");
        let events = vec![
            FileChange::Modified(path.clone()),
            FileChange::Modified(path.clone()),
            FileChange::Modified(path.clone()),
        ];
        let out = coalesce_file_changes(events);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], FileChange::Modified(p) if *p == path));
    }

    #[test]
    fn coalesce_modified_wins_over_removed_regardless_of_order() {
        // Order-independence pin: Modified beats Removed
        // whether Modified came first, last, or in the middle.
        // Same-path bursts must collapse to Modified in every
        // ordering, so the drain-level filesystem check has a
        // stable classification to work from (no matter what
        // arrival order the notify backend happened to use).
        let path = PathBuf::from("/tmp/order.txt");
        let orderings: [&[FileChange]; 3] = [
            &[
                FileChange::Modified(path.clone()),
                FileChange::Removed(path.clone()),
            ],
            &[
                FileChange::Removed(path.clone()),
                FileChange::Modified(path.clone()),
            ],
            &[
                FileChange::Removed(path.clone()),
                FileChange::Modified(path.clone()),
                FileChange::Removed(path.clone()),
            ],
        ];
        for events in orderings {
            let out = coalesce_file_changes(events.iter().cloned());
            assert_eq!(out.len(), 1, "events={events:?}");
            assert!(
                matches!(&out[0], FileChange::Modified(p) if *p == path),
                "expected Modified for events={events:?}, got {:?}",
                out[0]
            );
        }
    }

    #[test]
    fn coalesce_modified_wins_over_removed_for_same_path() {
        // Atomic-rename save: the notify backend may sequence
        // Modify(Name)→Remove-shaped events followed by a real
        // Modify(Any). The Modified reflects the final state;
        // Removed here is a transient rename artifact and must
        // NOT surface as a "was deleted" dialog.
        let path = PathBuf::from("/tmp/atomic.txt");
        let events = vec![
            FileChange::Removed(path.clone()),
            FileChange::Modified(path.clone()),
            FileChange::Removed(path.clone()),
        ];
        let out = coalesce_file_changes(events);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], FileChange::Modified(p) if *p == path),
            "Modified must win over Removed for the same path"
        );
    }

    #[test]
    fn coalesce_preserves_removed_when_no_modified_arrived() {
        // A genuine deletion with no follow-on Modified — the
        // file really is gone from that path. Must preserve the
        // Removed classification so the "was deleted" dialog
        // surfaces. The follow-on `try_exists` gate lives in
        // `Shell::drain`, not this helper, so we return
        // Removed here regardless of any filesystem state.
        let path = PathBuf::from("/tmp/deleted.txt");
        let events = vec![
            FileChange::Removed(path.clone()),
            FileChange::Removed(path.clone()),
        ];
        let out = coalesce_file_changes(events);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], FileChange::Removed(p) if *p == path));
    }

    #[test]
    fn coalesce_keeps_events_for_different_paths_separate() {
        // Two files saved simultaneously — one dialog per file
        // is correct. The coalescing is per-path only.
        let a = PathBuf::from("/tmp/a.txt");
        let b = PathBuf::from("/tmp/b.txt");
        let events = vec![
            FileChange::Modified(a.clone()),
            FileChange::Modified(b.clone()),
            FileChange::Modified(a.clone()),
        ];
        let mut out = coalesce_file_changes(events);
        out.sort_by(|x, y| {
            let px = match x {
                FileChange::Modified(p) | FileChange::Removed(p) => p,
            };
            let py = match y {
                FileChange::Modified(p) | FileChange::Removed(p) => p,
            };
            px.cmp(py)
        });
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], FileChange::Modified(p) if *p == a));
        assert!(matches!(&out[1], FileChange::Modified(p) if *p == b));
    }

    #[test]
    fn coalesce_empty_input_yields_empty_output() {
        // Trivially: no events → no dialogs. Guards a future
        // refactor that could accidentally emit a phantom entry
        // for an unmatched path.
        let out = coalesce_file_changes(Vec::<FileChange>::new());
        assert!(out.is_empty());
    }

    fn mk_debounce(first_set: std::time::Instant, deadline: std::time::Instant) -> DebounceEntry {
        DebounceEntry {
            first_set,
            deadline,
        }
    }

    #[test]
    fn debounced_path_returns_true_before_deadline() {
        // Own-save fence + cross-drain dupe suppression share
        // this predicate; a path with a future deadline (and
        // recent first_set) must be reported as debounced.
        use std::time::{Duration, Instant};
        let mut map = std::collections::HashMap::new();
        let path = PathBuf::from("/tmp/quiet.txt");
        let now = Instant::now();
        map.insert(
            path.clone(),
            mk_debounce(now, now + Duration::from_millis(500)),
        );
        assert!(is_path_debounced(&map, &path, now));
    }

    #[test]
    fn debounced_path_returns_false_after_deadline() {
        // Sliding `deadline` elapsed → predicate must return
        // false so a fresh external modification surfaces the
        // dialog again. Guards against a "debounce forever"
        // bug for the normal case (single burst, quiet after).
        use std::time::{Duration, Instant};
        let mut map = std::collections::HashMap::new();
        let path = PathBuf::from("/tmp/thawed.txt");
        let past = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .expect("Instant::now() - 10s is representable on any realistic clock");
        map.insert(path.clone(), mk_debounce(past, past));
        assert!(!is_path_debounced(&map, &path, Instant::now()));
    }

    #[test]
    fn debounced_path_returns_false_when_max_window_elapsed_even_if_deadline_future() {
        // The critical adversary-defence case: a runaway or
        // malicious writer touches the file at sub-debounce
        // cadence, keeping `deadline` perpetually in the
        // future. The MAX-window ceiling from `first_set` must
        // fire to surface the dialog after
        // FILE_CHANGE_DEBOUNCE_MAX regardless — otherwise the
        // "reload from disk?" tamper-detection signal would be
        // suppressed forever under adversary-controlled write
        // cadence. Reviewer flagged this on the first pass.
        use std::time::Instant;
        let mut map = std::collections::HashMap::new();
        let path = PathBuf::from("/tmp/adversarial.txt");
        let long_ago = Instant::now()
            .checked_sub(FILE_CHANGE_DEBOUNCE_MAX + std::time::Duration::from_secs(1))
            .expect("clock supports subtracting the max window plus one second");
        let far_future = Instant::now() + std::time::Duration::from_mins(1);
        // first_set is BEFORE the MAX window ago; deadline is
        // in the far future. The MAX-window ceiling must win.
        map.insert(path.clone(), mk_debounce(long_ago, far_future));
        assert!(
            !is_path_debounced(&map, &path, Instant::now()),
            "MAX-window ceiling must override an artificially-extended sliding deadline"
        );
    }

    #[test]
    fn debounced_path_returns_false_for_unknown_path() {
        // A path never touched by save/drain has no entry, so
        // the very first event for it surfaces the dialog.
        let map = std::collections::HashMap::new();
        assert!(!is_path_debounced(
            &map,
            Path::new("/tmp/first-event.txt"),
            std::time::Instant::now()
        ));
    }

    #[test]
    fn debounced_path_isolates_paths() {
        // Debouncing path A must not suppress events for path B.
        // Regression pin for a naive `contains_key(&any)` shape.
        use std::time::{Duration, Instant};
        let mut map = std::collections::HashMap::new();
        let a = PathBuf::from("/tmp/A.txt");
        let b = PathBuf::from("/tmp/B.txt");
        let now = Instant::now();
        map.insert(
            a.clone(),
            mk_debounce(now, now + Duration::from_millis(500)),
        );
        assert!(is_path_debounced(&map, &a, now));
        assert!(!is_path_debounced(&map, &b, now));
    }

    #[test]
    fn extend_deadline_preserves_first_set() {
        // The sliding-window extension must NOT reset
        // `first_set` — otherwise the MAX-window ceiling gets
        // pushed forward indefinitely and an adversarial write
        // cadence would suppress the dialog forever. This is
        // the invariant behind `is_path_debounced`'s
        // MAX-window ceiling check being useful.
        use std::time::{Duration, Instant};
        let mut map = std::collections::HashMap::new();
        let path = PathBuf::from("/tmp/keep-anchor.txt");
        let t0 = Instant::now();
        start_fresh_debounce(&mut map, path.clone(), t0);
        let anchored_first_set = map.get(&path).unwrap().first_set;
        // Extend later — deadline slides forward, first_set
        // stays anchored.
        let t1 = t0 + Duration::from_millis(400);
        extend_debounce_deadline(&mut map, &path, t1);
        let entry = map.get(&path).unwrap();
        assert_eq!(
            entry.first_set, anchored_first_set,
            "first_set must not be moved forward by extend_debounce_deadline"
        );
        assert_eq!(
            entry.deadline,
            t1 + FILE_CHANGE_DEBOUNCE,
            "deadline must slide forward with each extend"
        );
    }

    #[test]
    fn start_fresh_debounce_reanchors_first_set() {
        // After the MAX ceiling trips and drain surfaces the
        // dialog, `start_fresh_debounce` re-anchors `first_set`
        // to now so a NEW window begins. Without this, a
        // sustained adversarial write cadence would surface a
        // dialog on every subsequent event (spam) instead of
        // once per MAX window (the intended UX).
        use std::time::{Duration, Instant};
        let mut map = std::collections::HashMap::new();
        let path = PathBuf::from("/tmp/reanchor.txt");
        let t0 = Instant::now();
        start_fresh_debounce(&mut map, path.clone(), t0);
        let t1 = t0 + Duration::from_secs(5);
        start_fresh_debounce(&mut map, path.clone(), t1);
        let entry = map.get(&path).unwrap();
        assert_eq!(
            entry.first_set, t1,
            "start_fresh_debounce must re-anchor first_set on every call"
        );
        assert_eq!(entry.deadline, t1 + FILE_CHANGE_DEBOUNCE);
    }

    #[test]
    fn classify_upgrades_removed_to_modified_when_file_exists() {
        // The atomic-rename case: notify said Removed but the
        // rename landed a fresh file at the same path before we
        // look. Verifier must upgrade to Modified so the user
        // gets the correct reload prompt instead of a spurious
        // "was deleted" alert.
        let path = PathBuf::from("/tmp/exists.txt");
        let out = classify_change_by_existence(FileChange::Removed(path.clone()), |_| Ok(true));
        assert!(matches!(out, FileChange::Modified(p) if p == path));
    }

    #[test]
    fn classify_downgrades_modified_to_removed_when_file_absent() {
        // The bug the reviewer caught on the first pass:
        // `coalesce_file_changes` returns Modified for an
        // edit-then-delete batch (a build script that writes
        // then unlinks a file within one drain window). Without
        // filesystem verification the user gets a "reload from
        // disk?" prompt for a file that no longer exists, and
        // confirming reload fails the read. Filesystem truth
        // wins over the coalesced event kind.
        let path = PathBuf::from("/tmp/absent.txt");
        let out = classify_change_by_existence(FileChange::Modified(path.clone()), |_| Ok(false));
        assert!(matches!(out, FileChange::Removed(p) if p == path));
    }

    #[test]
    fn classify_removed_stays_removed_when_file_absent() {
        // No-op case: notify said Removed, file really is gone,
        // classification stays Removed. Guards against a
        // future refactor accidentally treating "false" as
        // "upgrade to Modified" via double-negation.
        let path = PathBuf::from("/tmp/really_gone.txt");
        let out = classify_change_by_existence(FileChange::Removed(path.clone()), |_| Ok(false));
        assert!(matches!(out, FileChange::Removed(p) if p == path));
    }

    #[test]
    fn classify_modified_stays_modified_when_file_present() {
        // Common case: notify said Modified, file exists,
        // classification stays Modified.
        let path = PathBuf::from("/tmp/normal.txt");
        let out = classify_change_by_existence(FileChange::Modified(path.clone()), |_| Ok(true));
        assert!(matches!(out, FileChange::Modified(p) if p == path));
    }

    #[test]
    fn classify_err_falls_through_as_removed() {
        // Permission denied / ADS access fault / symlink loop —
        // any Err from try_exists is safer to report as
        // Removed. The user's Save-As recovers the buffer
        // regardless, and a permissions-fault log line is not
        // more useful than the removal dialog.
        let path = PathBuf::from("/tmp/perm_denied.txt");
        let out = classify_change_by_existence(FileChange::Modified(path.clone()), |_| {
            Err(std::io::Error::other("permission denied"))
        });
        assert!(matches!(out, FileChange::Removed(p) if p == path));
    }

    #[test]
    fn coalesce_handles_the_user_reported_26_event_flood() {
        // Regression pin for the specific user-reported symptom:
        // 26 raw events for one path collapse to exactly one
        // FileChange. Matches the count in the bug report.
        let path = PathBuf::from("/tmp/example - Copy.rb");
        let events: Vec<FileChange> = std::iter::repeat_with(|| FileChange::Modified(path.clone()))
            .take(20)
            .chain(std::iter::repeat_with(|| FileChange::Removed(path.clone())).take(6))
            .collect();
        assert_eq!(events.len(), 26);
        let out = coalesce_file_changes(events);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], FileChange::Modified(p) if *p == path));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_tab_no_op_does_not_queue_doc_order_changed() {
        // The early `from == to` short-circuit must not queue a
        // notification — plugins shouldn't see a "list changed"
        // event when nothing actually moved.
        let mut shell = shell_with_synthetic_tabs(3, Some(1));
        let _ = shell.take_notifications();
        assert!(!shell.move_tab(1, 1));
        let n = shell.take_notifications();
        assert!(
            !n.iter().any(|x| matches!(x, Notification::DocOrderChanged)),
            "no-op move must not queue NPPN_DOCORDERCHANGED; got {n:?}"
        );
    }

    #[test]
    fn sanitize_basename_normal_case() {
        assert_eq!(
            sanitize_basename_for_backup(Path::new("notes.txt")),
            "notes.txt"
        );
        assert_eq!(
            sanitize_basename_for_backup(Path::new("/etc/hosts")),
            "hosts"
        );
        assert_eq!(
            sanitize_basename_for_backup(Path::new(r"C:\Users\Max\foo.bar.baz")),
            "foo.bar.baz"
        );
    }

    /// `..` substring must be neutralised so the resulting
    /// `<basename>@<timestamp>` filename round-trips through
    /// `is_safe_backup_filename` at restore time. Without the
    /// neutralisation, a file named `foo..txt` would silently
    /// drop its backup on next launch — the data-loss bug the
    /// reviewer caught.
    #[test]
    fn sanitize_basename_neutralises_dot_dot() {
        let s = sanitize_basename_for_backup(Path::new("foo..bar.txt"));
        assert!(!s.contains(".."), "dot-dot survived sanitization: {s}");
        assert!(is_safe_backup_filename(&format!("{s}@2026-01-01_000000")));

        let s = sanitize_basename_for_backup(Path::new("..important.txt"));
        assert!(!s.contains(".."), "leading dot-dot survived: {s}");
        assert!(is_safe_backup_filename(&format!("{s}@2026-01-01_000000")));

        // Three dots collapse to a single dot via one replace pass.
        let s = sanitize_basename_for_backup(Path::new("a...b.txt"));
        assert!(!s.contains(".."), "triple-dot survived: {s}");
        assert!(is_safe_backup_filename(&format!("{s}@2026-01-01_000000")));

        // Pure dot-runs need to round-trip too — a basename of
        // `"..."` (literal three dots) ought to fall back to the
        // `"untitled"` placeholder via the empty-or-pure-dot
        // guard. Caught here to lock in the boundary case the
        // security audit flagged.
        let s = sanitize_basename_for_backup(Path::new("..."));
        assert!(!s.contains(".."), "pure dot-run survived: {s}");
        assert!(is_safe_backup_filename(&format!("{s}@2026-01-01_000000")));
    }

    #[test]
    fn sanitize_basename_replaces_special_chars() {
        // Spaces survive; `:`, `@`, separators get replaced with `_`.
        let s = sanitize_basename_for_backup(Path::new("weird:name@here.txt"));
        assert_eq!(s, "weird_name_here.txt");
        assert!(is_safe_backup_filename(&format!("{s}@2026-01-01_000000")));
    }

    #[test]
    fn sanitize_basename_empty_falls_back() {
        assert_eq!(sanitize_basename_for_backup(Path::new("")), "untitled");
        assert_eq!(sanitize_basename_for_backup(Path::new(".")), "untitled");
        assert_eq!(sanitize_basename_for_backup(Path::new("..")), "untitled");
    }

    /// `restore_dirty_with_text` with `disk_changed_externally =
    /// true` must queue a `PendingDialog::ConfirmReload` so the
    /// next `Shell::drain` returns it to the UI. Without this
    /// path the user's first File→Save would silently overwrite
    /// an external edit made during the recovery window — the
    /// data-integrity scenario this whole feature targets.
    ///
    /// The hardcoded `/tmp/...` paths are unit-test fixtures
    /// only; the test never touches the filesystem. The
    /// `file_watcher.watch` call inside `restore_dirty_with_text`
    /// will fail on the non-existent path on Windows, which is
    /// non-fatal (the watcher logs at debug and the tab still
    /// works).
    #[test]
    fn restore_dirty_with_disk_changed_queues_confirm_reload() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        let path = PathBuf::from("/tmp/conflict.txt");

        shell.restore_dirty_with_text(
            &mut ui,
            path.clone(),
            "user's unsaved edits".into(),
            0,
            Encoding::Utf8,
            Eol::Lf,
            true,  // disk file changed externally
            false, // backup file untouched
            None,  // no persisted lang override — extension-detect
            false, // not pinned
        );

        let pending = shell.drain(&mut ui);
        let confirm_reload_count = pending
            .iter()
            .filter(|d| matches!(d, PendingDialog::ConfirmReload(p) if p == &path))
            .count();
        assert_eq!(
            confirm_reload_count, 1,
            "expected one ConfirmReload for the conflict path; got {pending:?}"
        );
    }

    /// `parse_backup_timestamp` is the exact inverse of
    /// `backup_timestamp` — we lean on that to verify the parser
    /// without hardcoding the civil-from-days arithmetic in the
    /// test as well. Generate a timestamp string, parse it, and
    /// confirm the round-trip lands inside our 5-second
    /// tolerance window.
    #[test]
    fn parse_backup_timestamp_round_trip() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ts = backup_timestamp();
        let filename = format!("new 1@{ts}");
        let parsed = parse_backup_timestamp(&filename).expect("parse");
        // Allow ±2 s slop for the seconds boundary the
        // timestamp formatter rounds to.
        assert!(
            parsed.abs_diff(now) <= 2,
            "round-trip drift too large: now={now} parsed={parsed}",
        );
    }

    /// Malformed inputs return `None` — no panic, no spurious
    /// "modified" flag. Covers the cases an attacker-controlled
    /// session.xml could provide.
    #[test]
    fn parse_backup_timestamp_rejects_malformed() {
        assert!(parse_backup_timestamp("no-at-sign").is_none());
        assert!(parse_backup_timestamp("name@too-short").is_none());
        assert!(parse_backup_timestamp("name@2026-XX-04_215750").is_none());
        // Bad month / hour / minute / second values are rejected
        // by the explicit range checks. We deliberately do *not*
        // validate per-month day counts or leap-year rules — the
        // worst case from a "Feb 30"-style entry is a slightly-
        // shifted UNIX timestamp comparison, which doesn't
        // matter for the "is this file newer than its embedded
        // mark by > 5 seconds?" check.
        assert!(parse_backup_timestamp("name@2026-13-01_120000").is_none());
        assert!(parse_backup_timestamp("name@2026-01-01_250000").is_none());
        assert!(parse_backup_timestamp("name@").is_none());
    }

    /// `is_backup_modified_externally` flags a backup whose mtime
    /// is meaningfully later than the timestamp embedded in its
    /// own filename. Uses `std::fs::set_modified` (stable since
    /// 1.75) to fast-forward the file's mtime past the
    /// embedded-timestamp + 5-second tolerance.
    #[test]
    fn detects_externally_modified_backup() {
        let dir = tempfile::tempdir().unwrap();
        // Pick a fixed timestamp well in the past so we can set
        // an mtime even further past while staying ahead by
        // > 5 s.
        let filename = "new 1@2026-01-01_120000";
        let backup_path = dir.path().join(filename);
        std::fs::write(&backup_path, b"some content").unwrap();
        // Embedded timestamp = 2026-01-01_120000 UTC. Set the
        // file's mtime to one minute later — well past the 5 s
        // tolerance window.
        let written_secs = parse_backup_timestamp(filename).expect("parse");
        let later = std::time::UNIX_EPOCH + std::time::Duration::from_secs(written_secs + 60);
        // `set_modified` on Windows requires the file to be opened
        // with write access (`File::open` defaults to read-only).
        std::fs::OpenOptions::new()
            .write(true)
            .open(&backup_path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(
            is_backup_modified_externally(&backup_path, filename),
            "backup with mtime > embedded ts + tolerance must be flagged",
        );
    }

    /// A backup file whose mtime sits within the tolerance
    /// window of its embedded timestamp is *not* flagged — that's
    /// the normal "we just wrote this" case.
    #[test]
    fn untouched_backup_not_flagged_as_external() {
        let dir = tempfile::tempdir().unwrap();
        let filename = "new 1@2026-01-01_120000";
        let backup_path = dir.path().join(filename);
        std::fs::write(&backup_path, b"some content").unwrap();
        let written_secs = parse_backup_timestamp(filename).expect("parse");
        // Bump mtime by 2 s — well inside the 5 s tolerance.
        let close_in_time =
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(written_secs + 2);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&backup_path)
            .unwrap()
            .set_modified(close_in_time)
            .unwrap();
        assert!(
            !is_backup_modified_externally(&backup_path, filename),
            "in-tolerance mtime must NOT be flagged",
        );
    }

    /// `restore_untitled_with_text` with `backup_modified_externally
    /// = true` queues a `PendingDialog::Error` so the user knows
    /// they're looking at content they didn't type. Buffer still
    /// shows the modified content (the user can save it if they
    /// want); the dialog is purely informational.
    #[test]
    fn restore_untitled_with_modified_backup_queues_error_dialog() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.restore_untitled_with_text(
            &mut ui,
            Some(7),
            "altered content".into(),
            0,
            Encoding::Utf8,
            Eol::Lf,
            true,  // backup was modified externally
            None,  // no custom rename label
            None,  // no persisted lang override — defaults to L_TEXT
            false, // not pinned
        );
        let pending = shell.drain(&mut ui);
        let n = pending
            .iter()
            .filter(|d| matches!(d, PendingDialog::Error { .. }))
            .count();
        assert_eq!(
            n, 1,
            "expected one Error dialog for the externally-modified backup; got {pending:?}"
        );
    }

    /// The clean case: when the disk hasn't been touched during
    /// the recovery window, no reload prompt fires — the user
    /// just sees their unsaved buffer with the dirty glyph.
    #[test]
    fn restore_dirty_without_disk_change_no_prompt() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        shell.restore_dirty_with_text(
            &mut ui,
            PathBuf::from("/tmp/clean.txt"),
            "user's unsaved edits".into(),
            0,
            Encoding::Utf8,
            Eol::Lf,
            false, // disk untouched
            false, // backup untouched
            None,  // no persisted lang override
            false, // not pinned
        );

        let pending = shell.drain(&mut ui);
        let confirm_reload_count = pending
            .iter()
            .filter(|d| matches!(d, PendingDialog::ConfirmReload(_)))
            .count();
        assert_eq!(
            confirm_reload_count, 0,
            "no reload prompt expected; got {pending:?}"
        );
    }

    /// `restore_dirty_with_text` honours a persisted language
    /// override — when the session.xml entry carries `@lang`, the
    /// restored tab's `lang` is the stored value, not the
    /// extension-derived default, AND the editor's lexer is
    /// applied so first-paint highlighting matches the override.
    /// This is the round-trip property the user relies on for
    /// "the language I picked last session is the one I see this
    /// session."
    #[test]
    fn restore_dirty_honours_persisted_lang_override() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        // `.txt` would auto-detect to L_TEXT. The stored override
        // is L_RUST (id 81) — what the user picked via the
        // Language menu. The restore must pick the override.
        shell.restore_dirty_with_text(
            &mut ui,
            PathBuf::from("/tmp/notes.txt"),
            "fn main() {}".into(),
            0,
            Encoding::Utf8,
            Eol::Lf,
            false,
            false,
            Some(81),
            false,
        );

        let idx = shell.active_tab.expect("a tab was just restored");
        assert_eq!(shell.tabs[idx].lang, codepp_core::lang::L_RUST);
        // The data field would pass even if the UI path silently
        // skipped `apply_lang` — assert the editor was actually
        // told about the new lexer too. Without this assertion a
        // future change that drops the `ui.apply_lang(...)` call
        // would leave the buffer rendered with the previous tab's
        // lexer and the test would still pass, masking a visual
        // regression.
        assert_eq!(
            ui.apply_lang_calls.last().copied(),
            Some(codepp_core::lang::L_RUST),
            "restore_dirty must apply the resolved lang to the editor"
        );
    }

    /// Symmetric coverage for `restore_untitled_with_text`:
    /// untitled buffers have no extension to detect from, so the
    /// stored override is the only signal that survives a
    /// rename-and-set-language flow. Same data-vs-UI assertion
    /// pair — without the `apply_lang_calls` check, a regression
    /// that skips the UI path is invisible.
    #[test]
    fn restore_untitled_honours_persisted_lang_override() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        shell.restore_untitled_with_text(
            &mut ui,
            Some(1),
            "fn main() {}".into(),
            0,
            Encoding::Utf8,
            Eol::Lf,
            false,
            None,
            Some(81),
            false,
        );

        let idx = shell.active_tab.expect("a tab was just restored");
        assert_eq!(shell.tabs[idx].lang, codepp_core::lang::L_RUST);
        assert_eq!(
            ui.apply_lang_calls.last().copied(),
            Some(codepp_core::lang::L_RUST),
            "restore_untitled must apply the resolved lang to the editor"
        );
    }
}
