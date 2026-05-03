# NPPM / NPPN coverage matrix

Authoritative source for which Notepad++ plugin messages Code++
implements. Updated on every commit that adds, expands, or
deprecates an `NPPM_*` or `NPPN_*` handler.

Status legend:

| Glyph | Meaning |
| --- | --- |
| ✅ | Implemented and exercised by at least one plugin in the test matrix. |
| 🟡 | Stub: returns sensible default (0 / NULL / empty), tracing-warn logged. Plugins that depend on the real semantics may break. |
| ⚫ | Not implemented; logged as `unhandled NPPM_*` and returns 0. |
| ⏸ | Deprecated upstream; Code++ ships a 🟡 stub for binary compat only. |

Phase tags reflect when each message is targeted for ✅:

- **v1** — Phase 3, the minimum needed to load and run the in-tree
  `example-hello` plugin and one unmodified small plugin from the
  wild.
- **v2** — Phase 4, find-in-files / lexer / encoding messages.
- **v3** — Phase 5, the long tail.

## Status as of Phase 3 milestone 3 (dispatcher landing)

The NPPM_*/NPPN_* dispatcher (`crates/plugin-host/src/dispatch.rs`)
is now in place: `dispatch_nppm` handles every v1 message below
against the `HostServices` trait, and `notify_all` synthesizes
`SCNotification` and broadcasts to every loaded plugin. **Rows
remain ⚫ until end-to-end-exercised by a real plugin DLL** in
`tools/npp-plugin-test/` per the legend; promotion to ✅ happens
once `shell` implements `HostServices` (milestone 4) and
`plugins/example-hello` exercises each call (milestone 5).

## NPPM_* (host-control)

| Message | Status | Phase | Notes |
| --- | --- | --- | --- |
| `NPPM_GETCURRENTSCINTILLA` | ⚫ | v1 | |
| `NPPM_GETCURRENTLANGTYPE` | ⚫ | v1 | Phase 3 returns `L_TEXT`; lang detection lands v2. |
| `NPPM_SETCURRENTLANGTYPE` | ⚫ | v1 | |
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
| `NPPM_SETSTATUSBAR` | ⚫ | v1 | |
| `NPPM_GETMENUHANDLE` | ⚫ | v1 | |
| `NPPM_ENCODESCI` | ⚫ | v2 | |
| `NPPM_DECODESCI` | ⚫ | v2 | |
| `NPPM_ACTIVATEDOC` | ⚫ | v1 | |
| `NPPM_LAUNCHFINDINFILESDLG` | ⚫ | v2 | |
| `NPPM_DMMSHOW` / `DMMHIDE` / `DMMUPDATEDISPINFO` / `DMMREGASDCKDLG` / `DMMVIEWOTHERTAB` / `DMMGETPLUGINHWNDBYNAME` | ⚫ | v3 | Docking-manager API, full set lands v3. |
| `NPPM_LOADSESSION` | ⚫ | v2 | |
| `NPPM_RELOADFILE` | ⚫ | v1 | |
| `NPPM_SWITCHTOFILE` | ⚫ | v1 | |
| `NPPM_SAVECURRENTFILE` | ⚫ | v1 | |
| `NPPM_SAVEALLFILES` | ⚫ | v2 | |
| `NPPM_SETMENUITEMCHECK` | ⚫ | v1 | |
| `NPPM_ADDTOOLBARICON` | ⚫ | v2 | |
| `NPPM_GETWINDOWSVERSION` | ⚫ | v1 | |
| `NPPM_MAKECURRENTBUFFERDIRTY` | ⚫ | v1 | |
| `NPPM_GETENABLETHEMETEXTUREFUNC` | ⏸ | — | |
| `NPPM_GETPLUGINSCONFIGDIR` | ⚫ | v1 | |
| `NPPM_MSGTOPLUGIN` | ⚫ | v3 | Inter-plugin messaging. |
| `NPPM_MENUCOMMAND` | ⚫ | v1 | |
| `NPPM_TRIGGERTABBARCONTEXTMENU` | ⚫ | v3 | |
| `NPPM_GETNPPVERSION` | ⚫ | v1 | Returns Code++'s version range-compatible with Notepad++ for plugin gating. |
| `NPPM_GETNPPDIRECTORY` | ⚫ | v1 | DESIGN.md §6.3 v1; not yet in compat header — add alongside `HostServices::program_dir`. |
| `NPPM_GETNPPFULLFILEPATH` | ⚫ | v1 | DESIGN.md §6.3 v1; not yet in compat header — add alongside `HostServices::program_path`. |
| `NPPM_HIDETABBAR` / `ISTABBARHIDDEN` | ⚫ | v2 | |
| `NPPM_GETPOSFROMBUFFERID` / `GETBUFFERIDFROMPOS` | ⚫ | v1 | |
| `NPPM_GETFULLPATHFROMBUFFERID` | ⚫ | v1 | |
| `NPPM_GETCURRENTBUFFERID` | ⚫ | v1 | |
| `NPPM_RELOADBUFFERID` | ⚫ | v2 | |
| `NPPM_GETBUFFERLANGTYPE` | ⚫ | v1 | |
| `NPPM_SETBUFFERLANGTYPE` | ⚫ | v2 | |
| `NPPM_GETBUFFERENCODING` / `SETBUFFERENCODING` | ⚫ | v2 | |
| `NPPM_GETBUFFERFORMAT` / `SETBUFFERFORMAT` | ⚫ | v2 | EOL style. |
| `NPPM_HIDETOOLBAR` / `ISTOOLBARHIDDEN` | ⚫ | v3 | |
| `NPPM_HIDEMENU` / `ISMENUHIDDEN` | ⚫ | v3 | |
| `NPPM_HIDESTATUSBAR` / `ISSTATUSBARHIDDEN` | ⚫ | v3 | |
| `NPPM_GETSHORTCUTBYCMDID` | ⚫ | v3 | |
| `NPPM_DOOPEN` | ⚫ | v1 | |
| `NPPM_SAVECURRENTFILEAS` | ⚫ | v2 | |
| Long tail (`NPPM_GETLANGUAGENAME` … `NPPM_GETZOOMLEVEL`) | ⚫ | v3 | |

## NPPN_* (notifications)

| Notification | Status | Phase | Notes |
| --- | --- | --- | --- |
| `NPPN_READY` | ⚫ | v1 | Fired after the first `setInfo`/`getFuncsArray` round trip. |
| `NPPN_TBMODIFICATION` | ⚫ | v2 | |
| `NPPN_FILEBEFORECLOSE` / `FILECLOSED` | ⚫ | v1 / v1 | |
| `NPPN_FILEBEFOREOPEN` / `FILEOPENED` | ⚫ | v2 / v1 | |
| `NPPN_FILEBEFORESAVE` / `FILESAVED` | ⚫ | v2 / v1 | |
| `NPPN_SHUTDOWN` | ⚫ | v1 | |
| `NPPN_BUFFERACTIVATED` | ⚫ | v1 | |
| `NPPN_LANGCHANGED` | ⚫ | v1 | |
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
