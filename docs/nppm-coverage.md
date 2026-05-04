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
| `NPPM_GETCURRENTLANGTYPE` | 🟡 | v1 | Returns `L_TEXT` (0). Real lang detection is v2 (lexer registry). |
| `NPPM_SETCURRENTLANGTYPE` | 🟡 | v1 | Returns `FALSE`; the host doesn't yet have a lang state to set. v2. |
| `NPPM_GETNBOPENFILES` | ⚫ | v2 | |
| `NPPM_GETOPENFILENAMES` | ⚫ | v2 | |
| `NPPM_MODELESSDIALOG` | ⚫ | v2 | |
| `NPPM_GETNBSESSIONFILES` | ⚫ | v2 | |
| `NPPM_GETSESSIONFILES` | ⚫ | v2 | |
| `NPPM_SAVESESSION` | ⚫ | v2 | |
| `NPPM_SAVECURRENTSESSION` | ⚫ | v2 | |
| `NPPM_GETOPENFILENAMESPRIMARY` | ⚫ | v2 | |
| `NPPM_GETOPENFILENAMESSECOND` | ⚫ | v3 | Split-view is v3 scope. |
| `NPPM_CREATESCINTILLAHANDLE` | ⚫ | v3 | Plugins that need their own Scintilla. |
| `NPPM_DESTROYSCINTILLAHANDLE` | ⏸ | — | Deprecated upstream; no-op. |
| `NPPM_GETNBUSERLANG` | ⚫ | v3 | |
| `NPPM_GETCURRENTDOCINDEX` | ⚫ | v2 | |
| `NPPM_SETSTATUSBAR` | ✅ | v1 | Wide-string `lparam` written into the requested status-bar part via `SB_SETTEXTW`. NUL-stripped before encoding. |
| `NPPM_GETMENUHANDLE` | ✅ | v1 | Returns the plugins-submenu HMENU (the one with per-plugin popups beneath it). Main-menu HMENU on request. |
| `NPPM_ENCODESCI` | ⚫ | v2 | |
| `NPPM_DECODESCI` | ⚫ | v2 | |
| `NPPM_ACTIVATEDOC` | 🟡 | v1 | Returns `TRUE` (single-tab fast path holds; multi-tab Phase 3 routes through `SWITCHTOFILE` so this remains a no-op true). |
| `NPPM_LAUNCHFINDINFILESDLG` | ⚫ | v2 | |
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
| `NPPM_RELOADBUFFERID` | ⚫ | v2 | |
| `NPPM_GETBUFFERLANGTYPE` | 🟡 | v1 | Returns `L_TEXT`. v2 wires through the lexer registry. |
| `NPPM_SETBUFFERLANGTYPE` | ⚫ | v2 | |
| `NPPM_GETBUFFERENCODING` / `SETBUFFERENCODING` | ⚫ | v2 | |
| `NPPM_GETBUFFERFORMAT` / `SETBUFFERFORMAT` | ⚫ | v2 | EOL style. |
| `NPPM_HIDETOOLBAR` / `ISTOOLBARHIDDEN` | ⚫ | v3 | |
| `NPPM_HIDEMENU` / `ISMENUHIDDEN` | ⚫ | v3 | |
| `NPPM_HIDESTATUSBAR` / `ISSTATUSBARHIDDEN` | ⚫ | v3 | |
| `NPPM_GETSHORTCUTBYCMDID` | ⚫ | v3 | |
| `NPPM_DOOPEN` | ✅ | v1 | Routes through `Shell::open_file`; same code path as the File→Open menu. |
| `NPPM_SAVECURRENTFILEAS` | ⚫ | v2 | |
| Long tail (`NPPM_GETLANGUAGENAME` … `NPPM_GETZOOMLEVEL`) | ⚫ | v3 | |

## NPPN_* (notifications)

| Notification | Status | Phase | Notes |
| --- | --- | --- | --- |
| `NPPN_READY` | 🟡 | v1 | `Notification::Ready` and the `NPPN_READY` code mapping exist; no fire site yet. The natural location is `PluginHost::load`, just after the `Ok(loaded)` branch records the plugin's `LoadedPlugin` — that puts the notification right after `setInfo` + `getFuncsArray` per the N++ contract. |
| `NPPN_TBMODIFICATION` | ⚫ | v2 | |
| `NPPN_FILEBEFORECLOSE` | 🟡 | v1 | Code mapping wired; not fired today (the close path queues `FILECLOSED` only). Adding the `BEFORE` fire site is a small follow-up. |
| `NPPN_FILECLOSED` | ✅ | v1 | Queued by `Shell::close_active_tab` after the data-model snapshot, fired after the `&mut Shell` borrow drops. |
| `NPPN_FILEBEFOREOPEN` | ⚫ | v2 | |
| `NPPN_FILEOPENED` | ✅ | v1 | Queued by `Shell::apply_load_result` on first successful load; deferred until after the borrow drops. |
| `NPPN_FILEBEFORESAVE` | ⚫ | v2 | |
| `NPPN_FILESAVED` | ✅ | v1 | Queued by `Shell::save_current_to_disk`. |
| `NPPN_SHUTDOWN` | ✅ | v1 | Fired by `ui_win32`'s `WM_DESTROY` handler before unload. |
| `NPPN_BUFFERACTIVATED` | ✅ | v1 | Queued on tab open, tab switch, tab close (when the new active tab differs), and `NPPM_SWITCHTOFILE` to a different open tab. Switch-to-already-active is suppressed. |
| `NPPN_LANGCHANGED` | 🟡 | v1 | Code mapping wired; no fire site (lang state isn't modeled until v2 lexers). |
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
