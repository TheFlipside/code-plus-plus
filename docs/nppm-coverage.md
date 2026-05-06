# NPPM / NPPN coverage matrix

Authoritative source for which Notepad++ plugin messages Code++
implements. Updated on every commit that adds, expands, or
deprecates an `NPPM_*` or `NPPN_*` handler.

Status legend:

| Glyph | Meaning |
| --- | --- |
| ✅ | Implemented and exercised by at least one plugin in the test matrix. |
| 🟡 | Stub: returns a sensible default (0 / NULL / empty / hardcoded), may trace-log. Plugins that depend on the real semantics may break. |
| ⚫ | Not implemented; logged as `unhandled NPPM_*` and returns 0. |
| ⏸ | Deprecated upstream; Code++ ships a 🟡 stub for binary compat only. |

Phase tags reflect when each message is targeted for ✅:

- **v1** — Phase 3, the minimum needed to load and run the in-tree
  `example-hello` plugin and one unmodified small plugin from the
  wild.
- **v2** — Phase 4, find-in-files / lexer / encoding messages.
- **v3** — Phase 5, the long tail.

## Status as of end of Phase 3

The NPPM_*/NPPN_* dispatcher (`crates/plugin-host/src/dispatch.rs`)
is in place. `dispatch_nppm` handles every v1 message below against
the `HostServices` trait that `shell` implements, and `notify_all`
synthesizes `SCNotification` and broadcasts to every loaded plugin.

**"Exercised" interpretation for the v1 cycle.** The strict reading
of the ✅ legend ("exercised by at least one plugin in the test
matrix") is per-message coverage in `tools/npp-plugin-test/`. That
automated coverage is a v2 (Phase 4) deliverable. For Phase 3 the
demo gate is `tools/npp-plugin-test/`'s in-tree `example-hello`
plus three real Notepad++ plugins from the wild loading and
running against the dispatcher: NppExport, mimeTools, and
NppConverter. A row is marked ✅ when the dispatcher's handler does
real work end-to-end and is reachable from any of those plugins'
typical code paths; it is marked 🟡 when the handler is a sensible
default that returns a hardcoded value or no-ops a state mutation
the host doesn't yet model (lexer language types, dirty-bit, etc.);
and ⚫ when there is no implementation. (Compare 1.5.5 was tested
but is a 32-bit DLL on a 64-bit host — `LoadLibraryW` rejects it
with `ERROR_BAD_EXE_FORMAT` and it does not exercise the dispatcher
at all, the same as in 64-bit Notepad++.)

## NPPM_* (host-control)

| Message | Status | Phase | Notes |
| --- | --- | --- | --- |
| `NPPM_GETCURRENTSCINTILLA` | ✅ | v1 | Out-writes the active view index (always 0 in single-view Phase 3). `example-hello` calls it before every `SCI_INSERTTEXT`. |
| `NPPM_GETCURRENTLANGTYPE` | ✅ | v1 | Returns the active tab's `LangType` (the v2 m1+m2 lexer wiring made this real). |
| `NPPM_SETCURRENTLANGTYPE` | ✅ | v1 | Sets the active tab's lang via `set_buffer_lang_type`; re-applies the lexer in the editor and queues `NPPN_LANGCHANGED` on a real change. |
| `NPPM_GETNBOPENFILES` | ✅ | v2 | wparam selector: `ALL_OPEN_FILES` / `PRIMARY_VIEW` / `SECOND_VIEW`. Phase 4 single-view: `ALL` and `PRIMARY` agree, `SECOND` is always 0. |
| `NPPM_GETOPENFILENAMES` | ✅ | v2 | Probe (wparam = NULL) returns the count without writing; otherwise writes up to `lparam` paths into the caller's TCHAR** array, capped at MAX_PATH per slot. Returns the number of slots actually written so the plugin can detect under-allocation. Untitled tabs (no on-disk path) are excluded — the array contract requires real paths. |
| `NPPM_MODELESSDIALOG` | ⚫ | v2 | |
| `NPPM_GETNBSESSIONFILES` | ⚫ | v2 | |
| `NPPM_GETSESSIONFILES` | ⚫ | v2 | |
| `NPPM_SAVESESSION` | ⚫ | v2 | |
| `NPPM_SAVECURRENTSESSION` | ⚫ | v2 | |
| `NPPM_GETOPENFILENAMESPRIMARY` | ✅ | v2 | Selector-fixed alias of `NPPM_GETOPENFILENAMES` against `PRIMARY_VIEW`. |
| `NPPM_GETOPENFILENAMESSECOND` | ✅ | v2 | Selector-fixed alias against `SECOND_VIEW` — always returns 0 / writes nothing in single-view Code++ (Phase 4). Real semantics land alongside split-view in Phase 5. |
| `NPPM_CREATESCINTILLAHANDLE` | ⚫ | v3 | Plugins that need their own Scintilla. |
| `NPPM_DESTROYSCINTILLAHANDLE` | ⏸ | — | Deprecated upstream; no-op. |
| `NPPM_GETNBUSERLANG` | ⚫ | v3 | |
| `NPPM_GETCURRENTDOCINDEX` | ✅ | v2 | wparam = view (0 = primary, 1 = secondary). Returns the active tab's index in `Shell.tabs` for primary, `-1` for secondary in single-view Code++ and for the no-active-tab case. |
| `NPPM_SETSTATUSBAR` | ✅ | v1 | Wide-string `lparam` written into the requested status-bar part via `SB_SETTEXTW`. NUL-stripped before encoding. |
| `NPPM_GETMENUHANDLE` | ✅ | v1 | Returns the plugins-submenu HMENU (the one with per-plugin popups beneath it). Main-menu HMENU on request. |
| `NPPM_ENCODESCI` | ⚫ | v2 | |
| `NPPM_DECODESCI` | ⚫ | v2 | |
| `NPPM_ACTIVATEDOC` | 🟡 | v1 | Returns `TRUE` (single-tab fast path holds; multi-tab Phase 3 routes through `SWITCHTOFILE` so this remains a no-op true). |
| `NPPM_LAUNCHFINDINFILESDLG` | ✅ | v2 | Opens the FIF tab in the Find/Replace dialog. `wparam` (wide path, optional) pre-fills the Directory combobox; `lparam` (wide string, optional) pre-fills Filters. Empty / NULL pointers leave the controls at their current values. |
| `NPPM_DMMSHOW` / `DMMHIDE` / `DMMUPDATEDISPINFO` / `DMMREGASDCKDLG` / `DMMVIEWOTHERTAB` / `DMMGETPLUGINHWNDBYNAME` | ⚫ | v3 | Docking-manager API, full set lands v3. |
| `NPPM_LOADSESSION` | ⚫ | v2 | |
| `NPPM_RELOADFILE` | ✅ | v1 | Routes through the same reload path the file-watcher uses; null `lparam` reloads the current buffer. |
| `NPPM_SWITCHTOFILE` | ✅ | v1 | Activates an already-open path; falls through to `open_file` if the path is not yet a tab. |
| `NPPM_SAVECURRENTFILE` | ✅ | v1 | Routes through `Shell::save_current_to_disk`. |
| `NPPM_SAVEALLFILES` | ⚫ | v2 | |
| `NPPM_SETMENUITEMCHECK` | 🟡 | v1 | Trace-logged; the full menu set (Edit/Search/View/…) lands in Phase 4, at which point this gets the real `CheckMenuItem` call. |
| `NPPM_ADDTOOLBARICON` | ⚫ | v2 | |
| `NPPM_GETWINDOWSVERSION` | 🟡 | v1 | Hardcoded `WV_WIN10 (16)`. Most plugins gate on `>= WV_WIN10`; Phase 4 swaps in `RtlGetVersion`. |
| `NPPM_MAKECURRENTBUFFERDIRTY` | 🟡 | v1 | Tracking lives in Scintilla (`SCI_GETMODIFY`); this currently just trace-logs. Title-bar dirty glyph is a Phase 4 concern. |
| `NPPM_GETENABLETHEMETEXTUREFUNC` | ⏸ | — | |
| `NPPM_GETPLUGINSCONFIGDIR` | ✅ | v1 | Wide-path write into the plugin's buffer, capped at `MAX_PATH` TCHARs. |
| `NPPM_MSGTOPLUGIN` | ⚫ | v3 | Inter-plugin messaging. |
| `NPPM_MENUCOMMAND` | 🟡 | v1 | Trace-logged; the dispatch table for built-in commands (IDM_FILE_OPEN etc.) lands alongside the full menu set in Phase 4. |
| `NPPM_TRIGGERTABBARCONTEXTMENU` | ⚫ | v3 | |
| `NPPM_GETNPPVERSION` | ✅ | v1 | Returns `CODEPP_PLUGIN_API_VERSION` (0.1, packed `(major << 16) \| minor`). Deliberately *below* any real Notepad++ release so version-gated N++ features (`if (NPPM_GETNPPVERSION() >= 0x00080000)` and the like) correctly fail their gate checks until Code++ implements those features. |
| `NPPM_GETNPPDIRECTORY` | ⚫ | v1 | Not yet in `dispatch.rs` constants; needs `HostServices::program_dir`. |
| `NPPM_GETNPPFULLFILEPATH` | ⚫ | v1 | Not yet in `dispatch.rs` constants; needs `HostServices::program_path`. |
| `NPPM_HIDETABBAR` / `ISTABBARHIDDEN` | ⚫ | v2 | |
| `NPPM_GETPOSFROMBUFFERID` / `GETBUFFERIDFROMPOS` | ⚫ | v1 | Pending; not yet in `dispatch.rs`. |
| `NPPM_GETFULLPATHFROMBUFFERID` | ✅ | v1 | Wide-path write capped at `MAX_PATH_TCHARS` (260); probe call (`lparam == 0`) always returns `MAX_PATH_TCHARS`, never the actual path length, so a plugin can't under-allocate based on the probe and overflow on the second call. |
| `NPPM_GETCURRENTBUFFERID` | ✅ | v1 | Returns the active tab's `BufferID` (sequential `i32`, base 1). |
| `NPPM_RELOADBUFFERID` | ✅ | v2 | wparam = buffer id, lparam = `BOOL` "alert before reload". Returns 1 on success, 0 for unknown id or no on-disk path. **Limitation:** `with_alert == true` reloads silently (without the user-confirmation prompt N++ shows), discarding any unsaved in-memory edits. Plugin-author warning: a workflow that relies on the alert to let the user abort (e.g. "discard and reload from VCS") will silently destroy unsaved work in Code++ until the dialog-routing wiring lands; a `tracing::warn!` fires in the host log when this code path is taken. |
| `NPPM_GETBUFFERLANGTYPE` | ✅ | v1 | Returns the per-tab `LangType` derived from the path extension on first load (and overridable by plugins via `NPPM_SETBUFFERLANGTYPE`). |
| `NPPM_SETBUFFERLANGTYPE` | ✅ | v1 | Mutates `Tab.lang`, re-applies the lexer when the tab is active, queues `NPPN_LANGCHANGED`. Idempotent on a same-lang set (no flicker, no false-positive notification). |
| `NPPM_GETBUFFERENCODING` | ✅ | v2 | Returns `UniMode`: `UNI_COOKIE` (UTF-8 without BOM), `UNI_UTF8` (UTF-8 with BOM), `UNI_UTF16LE`/`UNI_UTF16BE` (with BOM), `UNI_UTF16LE_NO_BOM`/`UNI_UTF16BE_NO_BOM`. `Encoding::Other` (unknown WHATWG codepage) collapses to `UNI_8BIT`. Pure 7-bit ASCII is reported as `UNI_COOKIE` (the detector folds ASCII into UTF-8); `UNI_7BIT` is defined for ABI completeness but never returned. `-1` for unknown buffer id. |
| `NPPM_SETBUFFERENCODING` | ⚫ | v2 | |
| `NPPM_GETBUFFERFORMAT` | ✅ | v2 | Returns `EolType`: `WIN_FORMAT`/`MAC_FORMAT`/`UNIX_FORMAT`. Code++'s internal `Eol::Mixed` reports `UNIX_FORMAT` (the Edit→EOL-Conversion default). `-1` for unknown buffer id. |
| `NPPM_SETBUFFERFORMAT` | ⚫ | v2 | |
| `NPPM_HIDETOOLBAR` / `ISTOOLBARHIDDEN` | ⚫ | v3 | |
| `NPPM_HIDEMENU` / `ISMENUHIDDEN` | ⚫ | v3 | |
| `NPPM_HIDESTATUSBAR` / `ISSTATUSBARHIDDEN` | ⚫ | v3 | |
| `NPPM_GETSHORTCUTBYCMDID` | ⚫ | v3 | |
| `NPPM_DOOPEN` | ✅ | v1 | Routes through `Shell::open_file`; same code path as the File→Open menu. |
| `NPPM_SAVECURRENTFILEAS` | ⚫ | v2 | |
| `NPPM_GETLANGUAGENAME` | ✅ | v1 | Wide-string write (probe-then-write protocol). Returns the short menu name for known langs ("C", "C++", "Rust", "Normal Text"); zero on unknown lang. |
| `NPPM_GETLANGUAGEDESC` | ✅ | v1 | Same shape as `NPPM_GETLANGUAGENAME`; returns the long human-readable description. |
| Long tail (`NPPM_ALLOCATESUPPORTED` … `NPPM_GETZOOMLEVEL`) | ⚫ | v3 | |

## NPPN_* (notifications)

| Notification | Status | Phase | Notes |
| --- | --- | --- | --- |
| `NPPN_READY` | ✅ | v1 | Fired at the just-loaded plugin only (per-plugin delivery in `PluginHost::load` right after `setInfo` + `getFuncsArray`). Code++'s lazy-load can't broadcast a single global ready like N++ does — per-plugin is the closest equivalent: each plugin sees READY exactly once at the moment it's actually ready to handle host messages. |
| `NPPN_TBMODIFICATION` | ⚫ | v2 | |
| `NPPN_FILEBEFORECLOSE` | 🟡 | v1 | Fired by `Shell::close_active_tab` ahead of `FILECLOSED` (N++ ordering). **Timing divergence (Phase 5 polish):** Code++'s notifications are queue-deferred — by the time a plugin's `beNotified(NPPN_FILEBEFORECLOSE)` runs, the tab has already been removed from `Shell.tabs`, so a callback into `NPPM_GETFULLPATHFROMBUFFERID(id)` returns -1 (unknown id). N++ delivers this notification synchronously while the buffer is still alive. Plugins that need the path at close time should cache it from the prior BUFFERACTIVATED. Synchronous-delivery wiring is tracked in DESIGN.md §7.4. |
| `NPPN_FILECLOSED` | ✅ | v1 | Queued by `Shell::close_active_tab` after the data-model snapshot, fired after the `&mut Shell` borrow drops. |
| `NPPN_FILEBEFOREOPEN` | ⚫ | v2 | |
| `NPPN_FILEOPENED` | ✅ | v1 | Queued by `Shell::apply_load_result` on first successful load; deferred until after the borrow drops. |
| `NPPN_FILEBEFORESAVE` | ⚫ | v2 | |
| `NPPN_FILESAVED` | ✅ | v1 | Queued by `Shell::save_current_to_disk`. |
| `NPPN_SHUTDOWN` | ✅ | v1 | Fired by `ui_win32`'s `WM_DESTROY` handler before unload. |
| `NPPN_BUFFERACTIVATED` | ✅ | v1 | Queued on tab open, tab switch, tab close (when the new active tab differs), and `NPPM_SWITCHTOFILE` to a different open tab. Switch-to-already-active is suppressed. |
| `NPPN_LANGCHANGED` | ✅ | v1 | Queued by `HostBridge::set_buffer_lang_type` on a real change (no-op same-lang sets are filtered out). Drained after the `&mut Shell` borrow drops, same pattern as the other lifecycle notifications. |
| Other (`WORDSTYLESUPDATED`, `SHORTCUTREMAPPED`, … `GLOBALMODIFIED`) | ⚫ | v3 | |

## How to update this matrix

When a commit promotes an entry from ⚫ → ✅:

1. Add or expand the integration test in `tools/npp-plugin-test/` so
   the test matrix exercises the message via a real plugin DLL.
2. Update this file's row.
3. The commit message references the message in the form
   `plugin-host: implement NPPM_GETCURRENTSCINTILLA (v1)`.

Demoting a status (✅ → 🟡 / ⚫) is a release-blocking bug per
[CLAUDE.md](../CLAUDE.md): "Plugin ABI freezes at Phase 3 completion
and never breaks."
