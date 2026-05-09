/*
 * Notepad_plus_msgs.h — NPPM_* host-control and NPPN_* notification
 * message numbers for Notepad++-compatible plugins.
 *
 * Part of Code++ (https://git.fiedler.live/tux/code-plus-plus).
 *
 * This header is an independent reimplementation of the Notepad++
 * plugin ABI. No source has been copied from Notepad++ or its plugin
 * SDK. The ABI surface defined here — message numbers, struct
 * layouts, function signatures, and behavior contracts — is not
 * protected by copyright; the original header source is, and is
 * therefore not used.
 *
 * Code++ is licensed under the MIT License. See LICENSE at the
 * repository root for the full text.
 *
 * Copyright (c) 2026 Max Fiedler and Code++ contributors.
 *
 * The constants below are organized by purpose. Coverage in Code++ is
 * staged: Phase 3 ships the messages tagged "v1" inline; later phases
 * fill in the rest. Plugins that send an unimplemented NPPM_* receive
 * 0 and a tracing warning is logged on the host side, so a plugin
 * compiled against this header always *links* — Code++ surfaces
 * unsupported features at runtime rather than at plugin-build time.
 *
 * The authoritative coverage matrix lives in `docs/nppm-coverage.md`.
 */

#ifndef CODEPP_NPPCOMPAT_NOTEPAD_PLUS_MSGS_H
#define CODEPP_NPPCOMPAT_NOTEPAD_PLUS_MSGS_H

#include <windows.h>

/*
 * Base message numbers.
 *
 *   NPPMSG    — host-control messages plugins SEND to the main window.
 *   NPPN_FIRST — notification codes Code++ delivers to plugins via
 *                beNotified()'s SCNotification.nmhdr.code.
 *
 * Both bases are stable across the entire Notepad++ plugin ecosystem;
 * Code++ matches them so existing binary plugins resolve our messages
 * to the same numbers they were compiled against.
 */
#define NPPMSG     (WM_USER + 1000)

/*
 * NPPN_FIRST is intentionally bare 1000 — *not* relative to WM_USER.
 * Notification codes travel through `SCNotification.nmhdr.code`, which
 * is a Scintilla-defined identifier space, not a Win32 message
 * number. Plugins compiled against the public ABI use 1000; matching
 * keeps the binary contract.
 */
#define NPPN_FIRST 1000

/* ------------------------------------------------------------------ */
/* NPPM_* — host-control messages (plugin -> Code++ main window)      */
/* ------------------------------------------------------------------ */

/* Buffer / view queries -------------------------------------------- */

/* v1: returns the HWND of the active Scintilla view. */
#define NPPM_GETCURRENTSCINTILLA          (NPPMSG + 4)
/* v1: returns the active buffer's lang-type id (Lang* enum). */
#define NPPM_GETCURRENTLANGTYPE           (NPPMSG + 5)
/* v1: sets the active buffer's lang-type id. */
#define NPPM_SETCURRENTLANGTYPE           (NPPMSG + 6)
/* v2: count of currently open files. wParam = selector
 *     (ALL_OPEN_FILES, PRIMARY_VIEW, SECOND_VIEW). Code++ is
 *     single-view through Phase 4: ALL and PRIMARY return the same
 *     count; SECOND is always 0. */
#define NPPM_GETNBOPENFILES               (NPPMSG + 7)
/* v2: fills wParam (TCHAR**) with full paths of open files.
 *     wParam: pointer to caller-allocated array of TCHAR* slots,
 *             each pointing to a buffer of at least MAX_PATH wide
 *             chars. NULL is a probe — host returns the count of
 *             files it would write.
 *     lParam: array capacity (slot count).
 *     The plain form always queries ALL_OPEN_FILES — there is no
 *     selector field on this message because wParam is consumed as
 *     the pointer. Use the -PRIMARY / -SECOND aliases below to
 *     restrict to a single view. Return value is the number of
 *     slots actually written, NOT the count attempted: a slot
 *     whose plugin-allocated pointer is NULL is logged and
 *     skipped, and the return reflects the gap. */
#define NPPM_GETOPENFILENAMES             (NPPMSG + 8)
/* v3: register or unregister a plugin-owned modeless-dialog
 *     HWND with the host's message pump. The pump consults each
 *     registered HWND via `IsDialogMessageW` so Tab / Enter /
 *     Esc / mnemonic handling works inside the dialog.
 *     wParam: MODELESSDIALOGADD (0) or MODELESSDIALOGREMOVE (1).
 *     lParam: HWND of the dialog.
 *     Returns: lParam on success (the upstream "echo HWND back
 *             so the call can chain" idiom), 0 on bad args.
 *     CONTRACT: the plugin owns the dialog's lifetime and MUST
 *     call REMOVE before destroying the HWND. The pump's
 *     `IsWindow` guard before each `IsDialogMessageW` call
 *     turns a forgotten REMOVE into a clean miss instead of
 *     UB on a freed handle, but plugins should not rely on
 *     that defensive backstop. */
#define NPPM_MODELESSDIALOG               (NPPMSG + 12)
/* Selectors for NPPM_MODELESSDIALOG's wParam. */
#ifndef MODELESSDIALOGADD
#define MODELESSDIALOGADD     0
#endif
#ifndef MODELESSDIALOGREMOVE
#define MODELESSDIALOGREMOVE  1
#endif

/*
 * Plugin-supplied session-write payload. Used by NPPM_SAVESESSION:
 * the plugin allocates this struct, fills in the destination path
 * and the file list, then sends a pointer to it as lParam.
 *
 * Layout matches Notepad++'s `sessionInfo` struct verbatim — same
 * field order, same C types — so a plugin that includes both this
 * header and the upstream N++ header sees the same wire format.
 *
 * Field semantics:
 *   - sessionFilePathName: wide path of the destination XML file.
 *     Must be non-null and null-terminated.
 *   - nbFile: count of valid entries in `files`. Negative values
 *     are rejected by the host; positive values are bounded at
 *     1024 (defensive cap against malformed plugin input).
 *   - files: array of `nbFile` wide-string pointers. Null entries
 *     are skipped without aborting the iteration.
 */
#ifndef NPP_SESSION_INFO_DEFINED
#define NPP_SESSION_INFO_DEFINED
typedef struct sessionInfo_ {
    TCHAR* sessionFilePathName;
    int    nbFile;
    TCHAR** files;
} sessionInfo;
#endif

/* Session ----------------------------------------------------------- */

/* v2: count of files in a session-XML at lParam.
 *     wParam: unused. lParam: TCHAR* path to a session file.
 *     Returns: file count, or 0 on parse failure (or for an empty
 *     session). The session schema is Code++'s `core::session`
 *     format; cross-tool reads of N++'s session.xml schema are
 *     Phase 5 polish. Untitled tabs are excluded — the message
 *     contract is "files," and untitled has no on-disk file. */
#define NPPM_GETNBSESSIONFILES            (NPPMSG + 13)
/* v2: write file paths from a session-XML into a plugin-allocated
 *     TCHAR** array.
 *     wParam: TCHAR** array of plugin-allocated buffers, each at
 *             least MAX_PATH wide chars.
 *     lParam: TCHAR* path to the session file.
 *     Returns: 1 on success, 0 on bad arguments / parse failure.
 *     Plugins should call NPPM_GETNBSESSIONFILES first to size the
 *     array. */
#define NPPM_GETSESSIONFILES              (NPPMSG + 14)
/* v2: write a session-XML containing the supplied file list.
 *     wParam: unused.
 *     lParam: pointer to a `sessionInfo` struct (path + count +
 *             TCHAR** array of file paths).
 *     Returns: lParam (the original pointer) on success — same
 *     "you can chain the call" idiom Notepad++ uses; 0 on bad
 *     args or write failure. The host enforces a count cap of
 *     1024 to bound the per-call allocation. */
#define NPPM_SAVESESSION                  (NPPMSG + 15)
/* v2: write a session-XML at lParam containing every currently-
 *     open titled buffer. Untitled tabs are excluded.
 *     wParam: unused. lParam: TCHAR* destination path.
 *     Returns: 1 on success, 0 on I/O failure. Foreign-tool
 *     readers (N++) won't pick the file up until cross-tool
 *     schema support lands as Phase 5 polish. */
#define NPPM_SAVECURRENTSESSION           (NPPMSG + 16)

/* Multi-view (split-view) — primary / secondary --------------------- */

/* v2: selector-fixed alias of NPPM_GETOPENFILENAMES — same arg
 *     shape, but always uses PRIMARY_VIEW regardless of any
 *     additional selector encoded by the caller. Predates the
 *     selector form on plain GETOPENFILENAMES. */
#define NPPM_GETOPENFILENAMESPRIMARY      (NPPMSG + 17)
/* v2: selector-fixed alias targeting SECOND_VIEW. Returns 0 on
 *     single-view Code++ (Phase 4); split-view is Phase 5 scope. */
#define NPPM_GETOPENFILENAMESSECOND       (NPPMSG + 18)

/* Scintilla handle management -------------------------------------- */

/* v3: create a fresh Scintilla control as a child of the
 *     plugin-supplied parent HWND (lParam). wParam is unused.
 *     Returns the new HWND as LRESULT, or 0 on failure (NULL /
 *     dead parent, CreateWindowExW failure). Plugin owns the
 *     new control's lifetime — it MUST DestroyWindow before
 *     the parent goes away. The control is initialised with
 *     SCI_SETCODEPAGE = SC_CP_UTF8 so it inherits the host's
 *     UTF-8-internal invariant; plugins are free to override
 *     that and any other Scintilla setting via direct
 *     SendMessage calls (or via SCI_GETDIRECTFUNCTION /
 *     SCI_GETDIRECTPOINTER for the hot-path direct-call API). */
#define NPPM_CREATESCINTILLAHANDLE        (NPPMSG + 20)
#define NPPM_DESTROYSCINTILLAHANDLE       (NPPMSG + 21)  /* deprecated upstream */
/* v3: number of user-defined languages (UDL) currently
 *     registered with the host. Code++ does not yet implement
 *     UDL — the menu, the XML parser, and the runtime registry
 *     are all Phase 4+ polish — so this is permanently 0 until
 *     UDL lands. Plugins gating on `if (NPPM_GETNBUSERLANG())`
 *     skip their UDL-aware code paths. */
#define NPPM_GETNBUSERLANG                (NPPMSG + 22)
/* v2: returns the active tab index in the requested view
 *     (wParam: 0 = primary, 1 = secondary). Returns -1 when the
 *     view has no active tab — including the secondary view in
 *     single-view Code++ (Phase 4). */
#define NPPM_GETCURRENTDOCINDEX           (NPPMSG + 23)

/* UI / menu / status bar ------------------------------------------- */

/* v1: lParam = const TCHAR*; sets the status-bar text. */
#define NPPM_SETSTATUSBAR                 (NPPMSG + 24)
/* v1: returns the menu HMENU for one of NPPMAINMENU / NPPPLUGINMENU. */
#define NPPM_GETMENUHANDLE                (NPPMSG + 25)
/* v2: convert the active buffer of the view at wParam (0 = primary,
 *     1 = secondary) to UTF-8 (no BOM). Code++'s Scintilla view is
 *     always UTF-8 internally; this is a metadata flip on the
 *     active tab's encoding, the same path as
 *     NPPM_SETBUFFERENCODING(id, UNI_COOKIE). Returns the new
 *     encoding numeric (UNI_COOKIE) on success, -1 if the view has
 *     no active buffer. Single-view Code++ (Phase 4) only has a
 *     primary view, so `wParam == 1` always returns -1. */
#define NPPM_ENCODESCI                    (NPPMSG + 26)
/* v2: inverse of NPPM_ENCODESCI — convert the active buffer of the
 *     view at wParam to single-byte ANSI. Same metadata-flip path
 *     as NPPM_SETBUFFERENCODING(id, UNI_8BIT); the on-disk encoding
 *     becomes the system codepage on the next save. Returns the
 *     new encoding numeric (UNI_8BIT) on success, -1 if the view
 *     has no active buffer. */
#define NPPM_DECODESCI                    (NPPMSG + 27)
/* v1: switches focus to the buffer at lParam (path). */
#define NPPM_ACTIVATEDOC                  (NPPMSG + 28)

/* Find-in-files dialog --------------------------------------------- */

/* Open the Find in Files dialog with optional pre-fill.
 * wParam: TCHAR* directory (or NULL to leave the field unchanged).
 * lParam: TCHAR* filters   (or NULL to leave the field unchanged).
 * An empty non-NULL string is treated identically to NULL — the
 * dispatcher folds the bad-surrogate path of `wide_ptr_to_string`
 * into the same "use current value" semantics so a single corrupt
 * arg can't trash a good prefill on the other arg. */
#define NPPM_LAUNCHFINDINFILESDLG         (NPPMSG + 29)

/* Docking / docked-dialog API (DM = "Docking Manager") -------------
 *
 * The host wraps the plugin's `hClient` HWND in a host-owned
 * floating frame; the plugin is responsible for the lifetime of
 * `hClient` (register before plugin shutdown, but DO NOT destroy
 * before the frame closes — the host re-parents the HWND into its
 * frame, so the plugin's normal "destroy on shutdown" cleanup is
 * fine). See `Docking.h` for the `tTbData` struct, the `DWS_*`
 * style flags, and the `DMN_*` notification codes. */

/* v3: show the floating frame previously registered for the
 *     `hClient` HWND in lParam (wParam unused). Returns 1 on
 *     success, 0 if the HWND isn't registered. */
#define NPPM_DMMSHOW                      (NPPMSG + 30)
/* v3: hide the floating frame previously registered for the
 *     `hClient` HWND in lParam (wParam unused). Registration
 *     survives — a subsequent NPPM_DMMSHOW re-shows. The user
 *     clicking the frame's X button routes through the same
 *     hide path (no DestroyWindow). Returns 1 on success, 0 if
 *     the HWND isn't registered. */
#define NPPM_DMMHIDE                      (NPPMSG + 31)
/* v3: refresh the floating frame's title / icon / add-info from
 *     the plugin's tTbData. wParam unused; lParam: registered
 *     hClient. Code++ floating-only mode (Phase 4 m4) returns
 *     success for any registered HWND but does **not** re-read
 *     the plugin's wide-string fields — the frame title stays
 *     as registered. Phase 5 docking-manager work re-reads the
 *     original tTbData pointer and refreshes everything. */
#define NPPM_DMMUPDATEDISPINFO            (NPPMSG + 32)
/* v3: register a plugin's HWND as a dockable dialog. wParam
 *     unused; lParam: pointer to a `tTbData` (see Docking.h).
 *     The host wraps `hClient` in a WS_OVERLAPPEDWINDOW |
 *     WS_EX_TOOLWINDOW frame, re-parents `hClient` into the
 *     frame's client area, and stores the registration entry.
 *     The frame is hidden until NPPM_DMMSHOW.
 *     Wide-string fields (pszName / pszModuleName / pszAddInfo)
 *     are read once at registration into host-side owned copies;
 *     the plugin's tTbData buffer can be freed after the call
 *     returns (though plugins typically keep it alive for the
 *     plugin's lifetime — N++ has the same convention).
 *     Returns 1 on success, 0 for null hClient / dead HWND /
 *     duplicate registration / frame-creation failure. */
#define NPPM_DMMREGASDCKDLG               (NPPMSG + 33)
/* v2: open every titled file listed in a session-XML at lParam,
 *     in the order they appear. The recorded active-tab is
 *     honoured implicitly (each open promotes the new tab to
 *     active). `WindowGeometry` and untitled-tab entries are
 *     ignored — those describe the recording tool's state, not
 *     state plugins are asking Code++ to adopt.
 *     wParam: unused. lParam: TCHAR* session path.
 *     Returns: 1 on a successful parse, 0 on read / parse
 *     failure. */
#define NPPM_LOADSESSION                  (NPPMSG + 34)
/* v3: switch to a sibling tab in the same docking container as the
 *     dialog whose name appears at lParam (wParam unused). Code++
 *     floating-only mode (Phase 4 m4) has no tab strip — every
 *     dock dialog is its own floating frame — so this is a no-op
 *     returning 0. Phase 5 docking-manager work activates it. */
#define NPPM_DMMVIEWOTHERTAB              (NPPMSG + 35)

/* File operations -------------------------------------------------- */

/* v1: reload the current buffer from disk. */
#define NPPM_RELOADFILE                   (NPPMSG + 36)
/* v1: switch to the buffer whose path is at lParam. */
#define NPPM_SWITCHTOFILE                 (NPPMSG + 37)
/* v1: save the active buffer. */
#define NPPM_SAVECURRENTFILE              (NPPMSG + 38)
/* v2: save every dirty titled buffer in one batch. Untitled tabs
 *     (no on-disk path) are skipped; per-tab errors are logged but
 *     don't abort the batch. Returns 1 unconditionally — per-file
 *     failures surface via the live error UI, not the ABI return. */
#define NPPM_SAVEALLFILES                 (NPPMSG + 39)

#define NPPM_SETMENUITEMCHECK             (NPPMSG + 40)
/* v3: add a plugin-supplied icon to the host toolbar bound to
 *     `wParam` (cmd id). A click on the new toolbar button
 *     posts WM_COMMAND with the plugin's cmd id — same path
 *     Code++'s built-in toolbar buttons use.
 *     wParam: cmd id.
 *     lParam: pointer to a `toolbarIcons` struct
 *             ({ HBITMAP hToolbarBmp; HICON hToolbarIcon; }).
 *     Returns: TRUE on success, FALSE on null icon / imagelist
 *             failure / TB_ADDBUTTONS rejection.
 *     **Code++ uses `hToolbarIcon` only.** The legacy
 *     `hToolbarBmp` (16x16 16-color bitmap, kept for old
 *     Win9x-era N++) is logged-and-ignored — modern plugins
 *     ship the 32-bpp HICON. Plugins owning only the legacy
 *     bitmap get a FALSE return and should fall back to
 *     installing a menu-only command. The plugin owns the
 *     HICON's lifetime; `ImageList_ReplaceIcon` internally
 *     copies the bits, so the plugin can free its handle
 *     immediately after this call returns. */
#define NPPM_ADDTOOLBARICON               (NPPMSG + 41)
/* Plugin-supplied icon payload for `NPPM_ADDTOOLBARICON`. */
#ifndef NPP_TOOLBAR_ICONS_DEFINED
#define NPP_TOOLBAR_ICONS_DEFINED
typedef struct toolbarIcons_ {
    HBITMAP hToolbarBmp;     /* legacy 16-color bitmap (ignored by Code++) */
    HICON   hToolbarIcon;    /* 32-bpp icon (preferred) */
} toolbarIcons;
#endif
/* v1: returns Notepad++'s `winVer` enum value for the running OS
 *     (... WV_WIN10 = 16, WV_WIN11 = 17). Probed via `RtlGetVersion`
 *     so the binary doesn't need a manifest declaring Win11 support
 *     to read the real kernel version. Falls back to `WV_WIN10` if
 *     the probe fails. */
#define NPPM_GETWINDOWSVERSION            (NPPMSG + 42)
/* v3: look up a registered docking dialog's `hClient` by display
 *     name. wParam: TCHAR* module name (or NULL = match any
 *     module's registration). lParam: TCHAR* dialog name
 *     (required, the value the registering tTbData supplied as
 *     pszName). Returns the matching hClient HWND, or 0 if no
 *     entry matches. Plugins use this to discover whether
 *     another plugin's docked dialog is up — the standard
 *     dependency-discovery pattern in N++. */
#define NPPM_DMMGETPLUGINHWNDBYNAME       (NPPMSG + 43)
#define NPPM_MAKECURRENTBUFFERDIRTY       (NPPMSG + 44)
#define NPPM_GETENABLETHEMETEXTUREFUNC    (NPPMSG + 45)  /* deprecated upstream */

/* v1: returns the path of the per-user plugin config dir (TCHAR* via lParam). */
#define NPPM_GETPLUGINSCONFIGDIR          (NPPMSG + 46)
/* v3: forward an inter-plugin message to a named target.
 *     wParam: TCHAR* target plugin name (the value the target's
 *             getName() returned).
 *     lParam: pointer to a `CommunicationInfo` struct (path
 *             defined below). The host reads `internal_msg` and
 *             calls `target.messageProc(internal_msg, info_ptr, 0)`,
 *             returning the LRESULT verbatim.
 *     Returns: target's messageProc return value, or 0 if the
 *             target plugin is not loaded / name doesn't match. */
#define NPPM_MSGTOPLUGIN                  (NPPMSG + 47)

/*
 * Plugin-to-plugin messaging payload. Used by NPPM_MSGTOPLUGIN.
 * The source plugin populates the three fields and passes a
 * pointer to this struct in lParam; the host reads only
 * `internalMsg` and forwards the struct pointer to the target's
 * messageProc as wParam. Layout matches Notepad++'s upstream
 * `CommunicationInfo` verbatim.
 */
#ifndef NPP_COMMUNICATION_INFO_DEFINED
#define NPP_COMMUNICATION_INFO_DEFINED
typedef struct CommunicationInfo_ {
    long         internalMsg;     /* custom message code chosen by sender */
    const TCHAR* srcModuleName;   /* sender's getName() value */
    void*        info;            /* sender-defined opaque payload */
} CommunicationInfo;
#endif
/* v1: invoke the menu command identified by lParam (cmdID). */
#define NPPM_MENUCOMMAND                  (NPPMSG + 48)
/* v3: open the tab-bar context menu programmatically.
 *     wParam: view (0 = primary, 1 = secondary).
 *     lParam: tab index (or -1 for "use the active tab").
 *     Returns: BOOL — TRUE if the menu opened.
 *     **Phase 4 polish (DESIGN.md §7.4):** Code++'s tab strip
 *     has no context menu yet (no Close / Close-Others /
 *     Move-to-other-view / Rename / Delete-from-disk entries),
 *     so this is currently a no-op returning FALSE. */
#define NPPM_TRIGGERTABBARCONTEXTMENU     (NPPMSG + 49)
/* v1: returns Code++'s self-reported version, encoded high-word major,
 *     low-word minor — chosen to be range-compatible with Notepad++ for
 *     plugin gating that does `if (NPPM_GETNPPVERSION() >= ...)`. */
#define NPPM_GETNPPVERSION                (NPPMSG + 50)

/* v2: hide or show the tab strip. wParam = BOOL (TRUE hides,
 *     FALSE shows). Returns the *previous* hidden state so a
 *     plugin can detect "I just changed it" (return != wparam)
 *     vs. "it was already in this state". The Scintilla view
 *     auto-resizes to fill the freed space when hidden, and
 *     shrinks back to its original height when shown — driven
 *     by a deferred WM_SIZE PostMessage to avoid re-entering
 *     wnd_proc under PluginCallGuard. */
#define NPPM_HIDETABBAR                   (NPPMSG + 51)
/* v2: returns BOOL — current tab-strip hidden state. */
#define NPPM_ISTABBARHIDDEN               (NPPMSG + 52)

/* Buffer-id queries ------------------------------------------------ */

/* v1: tab-strip position of the buffer whose id is in wParam.
 *     wParam: buffer id.
 *     lParam: priority view selector (0 = main, 1 = sub) — advisory
 *             in single-view Code++; Phase 5 split-view will honour
 *             it.
 *     Returns the tab index, with bit 0x40000000 set if the buffer
 *     lives in the secondary view. -1 if the buffer id is unknown. */
#define NPPM_GETPOSFROMBUFFERID           (NPPMSG + 57)
/* v1: lParam (TCHAR*) is filled with the full path of the buffer
 *     whose id is in wParam. */
#define NPPM_GETFULLPATHFROMBUFFERID      (NPPMSG + 58)
/* v1: buffer id of the tab at the requested view-relative position.
 *     wParam: tab position (0-based index into the view's tab list).
 *     lParam: view selector (0 = main, 1 = sub).
 *     Returns the buffer id, or 0 (N++'s "no buffer" sentinel) if
 *     the position is out of range, the view is unknown, or — in
 *     single-view Code++ — `view == 1`. */
#define NPPM_GETBUFFERIDFROMPOS           (NPPMSG + 59)
/* v1: returns the active buffer's id (LRESULT). */
#define NPPM_GETCURRENTBUFFERID           (NPPMSG + 60)
/* v2: reload the buffer at wParam (buffer id) from disk, blowing
 *     away in-memory edits.
 *     wParam: buffer id.
 *     lParam: BOOL — TRUE asks for the "modified externally —
 *             reload?" confirmation, FALSE for a silent reload.
 *     **Phase 4 limitation:** Code++ silently reloads regardless of
 *     the alert flag; the dialog-routing wiring lands in a later
 *     polish. Plugins passing TRUE see a `tracing::warn!` so the
 *     gap is visible in the host log.
 *     Returns 1 on success (id resolved to a path and reload was
 *     issued), 0 if the id is unknown or has no on-disk path. */
#define NPPM_RELOADBUFFERID               (NPPMSG + 61)
/* v1: returns the lang-type id for the buffer at wParam. */
#define NPPM_GETBUFFERLANGTYPE            (NPPMSG + 64)
#define NPPM_SETBUFFERLANGTYPE            (NPPMSG + 65)
/* v2: returns the encoding (UniMode enum) of the buffer at wParam.
 *     -1 if the buffer id is unknown — distinct from UNI_8BIT (0)
 *     so plugins can tell "no such buffer" from "8-bit buffer".
 *     Code++'s `Encoding::Other(label)` (an unknown WHATWG codepage
 *     such as `windows-1252` or `shift_jis`) collapses to UNI_8BIT
 *     because N++'s ABI carries no codepage identity past this
 *     return value either. */
#define NPPM_GETBUFFERENCODING            (NPPMSG + 66)
#define NPPM_SETBUFFERENCODING            (NPPMSG + 67)
/* v2: returns the EOL format (EolType enum) of the buffer at wParam.
 *     -1 if the buffer id is unknown — same separation rationale as
 *     NPPM_GETBUFFERENCODING. Code++'s internal `Eol::Mixed`
 *     (per-line preservation when the file's EOL is inconsistent)
 *     reports UNIX_FORMAT (LF) since that is what the
 *     "Edit -> EOL Conversion" normalisation picks. */
#define NPPM_GETBUFFERFORMAT              (NPPMSG + 68)
#define NPPM_SETBUFFERFORMAT              (NPPMSG + 69)

/* UI chrome toggles ------------------------------------------------ */

/* v2: hide / show the toolbar.
 *     wParam: BOOL (TRUE hides, FALSE shows).
 *     Returns: the *previous* hidden state — same contract shape
 *     as `NPPM_HIDETABBAR`. The editor area auto-relayouts to fill
 *     the freed band (Win32 routes via deferred `WM_SIZE`). */
#define NPPM_HIDETOOLBAR                  (NPPMSG + 70)
/* v2: BOOL — current toolbar hidden state. */
#define NPPM_ISTOOLBARHIDDEN              (NPPMSG + 71)
/* v2: hide / show the main menu bar. Win32 swaps via
 *     `SetMenu(NULL)` / `SetMenu(main_menu)` + `DrawMenuBar`.
 *     Same wParam / return contract as `NPPM_HIDETOOLBAR`. */
#define NPPM_HIDEMENU                     (NPPMSG + 72)
/* v2: BOOL — current menu bar hidden state. */
#define NPPM_ISMENUHIDDEN                 (NPPMSG + 73)
/* v2: hide / show the status bar. Same wParam / return contract. */
#define NPPM_HIDESTATUSBAR                (NPPMSG + 74)
/* v2: BOOL — current status bar hidden state. */
#define NPPM_ISSTATUSBARHIDDEN            (NPPMSG + 75)

/* v3: look up the keyboard shortcut bound to a built-in command
 *     id. wParam: cmd id. lParam: ShortcutKey* OUT — host
 *     writes the binding's Ctrl/Alt/Shift bits + virtual key.
 *     Returns: BOOL — TRUE if a binding exists, FALSE
 *     otherwise (out-buffer untouched on FALSE). */
#define NPPM_GETSHORTCUTBYCMDID           (NPPMSG + 76)
/* v1: open the file at lParam (TCHAR*). */
#define NPPM_DOOPEN                       (NPPMSG + 77)
/* v2: save the active buffer to a new path.
 *     wParam: BOOL — TRUE writes a copy without re-pointing the
 *             active tab; FALSE renames the active tab to the new
 *             path and subsequent saves write there.
 *     lParam: TCHAR* destination path.
 *     Returns: 1 on success, 0 on I/O / encoding failure or when
 *             there is no active buffer. */
#define NPPM_SAVECURRENTFILEAS            (NPPMSG + 78)
#define NPPM_GETCURRENTNATIVELANGENCODING (NPPMSG + 79)
/* v3: BOOL — `TRUE` when the host supports
 *     `NPPM_ALLOCATECMDID` / `NPPM_ALLOCATEMARKER` (the
 *     plugin-driven id reservation messages). Plugins gate
 *     `if (NPPM_ALLOCATESUPPORTED) { … }` on this. Code++ now
 *     supports both allocators — this returns `TRUE`. */
#define NPPM_ALLOCATESUPPORTED            (NPPMSG + 80)
/* v3: reserve `wParam` consecutive plugin-command IDs.
 *     wParam: count requested.
 *     lParam: int* OUT — host writes the starting id.
 *     Returns: TRUE on success (plugin then uses ids
 *             `*lParam .. *lParam + wParam`),
 *             FALSE on bad args (count <= 0, NULL out) or pool
 *             exhaustion. Code++ pool: 60_000..65_500 (5500 ids).
 *             Allocations are durable for the host's lifetime. */
#define NPPM_ALLOCATECMDID                (NPPMSG + 81)
/* v3: reserve `wParam` consecutive Scintilla marker numbers.
 *     Same shape as `NPPM_ALLOCATECMDID`. Code++ pool: 25..=31
 *     (seven markers above the bookmark slot at 24). */
#define NPPM_ALLOCATEMARKER               (NPPMSG + 82)
#define NPPM_GETLANGUAGENAME              (NPPMSG + 83)
#define NPPM_GETLANGUAGEDESC              (NPPMSG + 84)
/* v3: show or hide the doc-switcher panel.
 *     wParam: BOOL (TRUE shows, FALSE hides).
 *     Returns: previous shown state.
 *     Code++ has no doc-switcher panel; the call is a no-op and
 *     always returns FALSE. */
#define NPPM_SHOWDOCSWITCHER              (NPPMSG + 85)
/* v3: BOOL — doc-switcher currently shown? Code++ has no panel,
 *     so this is permanently FALSE. */
#define NPPM_ISDOCSWITCHERSHOWN           (NPPMSG + 86)
/* v3: BOOL — `TRUE` when plugins installed under
 *     `%APPDATA%\Code++\plugins` are honoured (per-user, no admin
 *     restriction). Code++ always loads from the per-user dir, so
 *     this is unconditionally `TRUE`. */
#define NPPM_GETAPPDATAPLUGINSALLOWED     (NPPMSG + 87)
/* v3: returns the active view index (0 = primary, 1 = secondary).
 *     Code++ is single-view through Phase 4 → always 0. */
#define NPPM_GETCURRENTVIEW               (NPPMSG + 88)
/* v3: disable a column in the doc-switcher's listview.
 *     wParam: column index. lParam: BOOL disable flag.
 *     Code++ has no doc-switcher panel; the call is a no-op and
 *     always returns 0. */
#define NPPM_DOCSWITCHERDISABLECOLUMN     (NPPMSG + 89)
/* v3: returns the editor's default foreground colour as a Win32
 *     `COLORREF` (`0x00BBGGRR`). Reads
 *     `SCI_STYLEGETFORE(STYLE_DEFAULT)` on the active editor. */
#define NPPM_GETEDITORDEFAULTFOREGROUNDCOLOR (NPPMSG + 90)
/* v3: returns the editor's default background colour. Same
 *     `COLORREF` layout as the foreground peer. */
#define NPPM_GETEDITORDEFAULTBACKGROUNDCOLOR (NPPMSG + 91)
/* v3: toggle Scintilla's font-rendering quality.
 *     wParam: BOOL (TRUE = LCD-optimised / ClearType,
 *             FALSE = non-antialiased).
 *     Returns: previous state. */
#define NPPM_SETSMOOTHFONT                (NPPMSG + 92)
/* v3: toggle the Scintilla view's `WS_EX_CLIENTEDGE` border.
 *     wParam: BOOL. Returns previous state. */
#define NPPM_SETEDITORBORDEREDGE          (NPPMSG + 93)
/* v3: save the buffer matching `lParam` (TCHAR* path) to disk.
 *     Returns 1 on success, 0 on bad args / unknown path / write
 *     failure. **Phase 4 limitation:** Code++ saves only the
 *     ACTIVE tab through this path; cross-tab save needs
 *     doc-pointer-swap UI cooperation tracked in DESIGN.md §7.4. */
#define NPPM_SAVEFILE                     (NPPMSG + 94)
/* v3: tell the host to suppress auto-update prompts.
 *     wParam / lParam: unused. Code++ has no auto-update; the
 *     call is unconditionally a no-op. */
#define NPPM_DISABLEAUTOUPDATE            (NPPMSG + 95)
/* v3: remove every accelerator-table binding for the given
 *     cmd id.
 *     wParam: cmd id whose binding to drop.
 *     lParam: unused.
 *     Returns: TRUE if a binding existed and was removed, FALSE
 *             otherwise (unknown cmd id, or no binding to
 *             remove). After a successful REMOVE, calling
 *             NPPM_GETSHORTCUTBYCMDID for the same cmd id
 *             returns FALSE — the binding is gone from the live
 *             accelerator table. Win32 has no in-place mutation
 *             API, so the host implements REMOVE as
 *             copy → filter → create-new → destroy-old → swap
 *             on the HACCEL it stores; the message pump reads
 *             the live handle on every iteration so the change
 *             takes effect on the very next keystroke. */
#define NPPM_REMOVESHORTCUTBYCMDID        (NPPMSG + 96)
/* v3: returns the per-user plugins directory (parent of every
 *     plugin subdirectory) into a plugin-allocated wide buffer.
 *     wParam: capacity in TCHARs. lParam: TCHAR* OUT.
 *     Returns 1 on success, 0 on bad args / unresolvable config
 *     dir (sandboxed runner). Same out-buffer protocol as
 *     `NPPM_GETPLUGINSCONFIGDIR`. */
#define NPPM_GETPLUGINHOMEPATH            (NPPMSG + 97)
/* v3: returns the settings cloud-sync directory if the user opted
 *     in. Same out-buffer protocol; Code++ does not implement
 *     cloud-sync, so the dispatcher writes an empty wide string
 *     (just the NUL terminator) and returns 1 (the path lookup
 *     itself didn't fail; the user simply has no cloud sync
 *     configured). */
#define NPPM_GETSETTINGSCLOUDPATH         (NPPMSG + 98)
/* v3: set the line-number margin width mode (DYNAMIC / CONSTANT).
 *     wParam: one of `LINENUMWIDTH_DYNAMIC` (0) or
 *             `LINENUMWIDTH_CONSTANT` (1).
 *     Returns 1 on accepted value, 0 on unknown. **Phase 4
 *     polish:** Code++ records the request but the visible
 *     margin always uses dynamic width until the constant-mode
 *     pin lands. */
#define NPPM_SETLINENUMBERWIDTHMODE       (NPPMSG + 99)
/* v3: returns the current line-number margin width mode. Code++
 *     reports `LINENUMWIDTH_DYNAMIC` until the constant-mode pin
 *     lands. */
#define NPPM_GETLINENUMBERWIDTHMODE       (NPPMSG + 100)

#ifndef LINENUMWIDTH_DYNAMIC
#define LINENUMWIDTH_DYNAMIC  0
#endif
#ifndef LINENUMWIDTH_CONSTANT
#define LINENUMWIDTH_CONSTANT 1
#endif
/* v3: returns the Scintilla marker number reserved for bookmarks.
 *     Code++ uses N++'s convention of marker 24, so plugins that
 *     install a bookmark via `SCI_MARKERADD(line, 24)` work the
 *     same way they would in N++. (Code++'s UI does not yet style
 *     marker 24 as a visible bookmark glyph — Phase 4 polish —
 *     but the marker is set on the buffer correctly.) */
#define NPPM_GETBOOKMARKID                (NPPMSG + 101)
/* v3: returns the active editor's zoom level in points (Scintilla
 *     `SCI_GETZOOM`). Range is approximately [-10, 20]. */
#define NPPM_GETZOOMLEVEL                 (NPPMSG + 102)

/* Dark-mode query family. Plugins observe a system theme flip
 * via `NPPN_DARKMODECHANGED` then re-read the live host state
 * via these two queries. Code++ Phase 4 has no host-side dark
 * mode yet (DESIGN.md §7.4), so both queries return FALSE; the
 * notifications still fire so dark-mode-aware plugins can react
 * to the system flip immediately. */

/* v3: returns BOOL — TRUE iff the host is currently rendering
 *     its own chrome in dark mode. The user can override the
 *     system theme independently of the system setting, so
 *     plugins should re-read this on every NPPN_DARKMODECHANGED
 *     rather than caching from the system signal alone.
 *     Code++ Phase 4: always FALSE. */
#define NPPM_ISDARKMODEENABLED            (NPPMSG + 110)

/* Plugin-side payload struct for NPPM_GETDARKMODECOLORS — 12 ×
 * COLORREF (each `0x00BBGGRR`, 4 bytes), 48 bytes total on
 * every platform. Plugins allocate one instance and pass its
 * size + pointer in wParam / lParam. */
#ifndef NPP_DARK_MODE_COLORS_DEFINED
#define NPP_DARK_MODE_COLORS_DEFINED
typedef struct NppDarkModeColors_ {
    COLORREF background;
    COLORREF ctrlBackground;
    COLORREF hotBackground;
    COLORREF dlgBackground;
    COLORREF errorBackground;
    COLORREF text;
    COLORREF darkerText;
    COLORREF disabledText;
    COLORREF linkText;
    COLORREF edge;
    COLORREF hotEdge;
    COLORREF disabledEdge;
} NppDarkModeColors;
#endif

/* v3: write the host's dark-mode palette into the plugin's
 *     `NppDarkModeColors` buffer.
 *     wParam: sizeof(NppDarkModeColors) — the host validates
 *             against its own `sizeof` and refuses to write
 *             on mismatch (layout-drift defence).
 *     lParam: pointer to the plugin's NppDarkModeColors.
 *     Returns: BOOL — TRUE if 12 COLORREFs were written,
 *             FALSE otherwise. Code++ Phase 4: always FALSE
 *             (no dark mode active → no palette to share).
 *             Plugins that gate on NPPM_ISDARKMODEENABLED
 *             skip the call and never observe the gap. */
#define NPPM_GETDARKMODECOLORS            (NPPMSG + 111)

/*
 * RUNCOMMAND_USER family. Notepad++ split a handful of host-state-
 * as-environment queries (the application's directory, the running
 * executable's full path, ...) into a separate base at WM_USER+3000
 * rather than tucking them inside the main NPPMSG range. Code++
 * mirrors the base value so plugins compiled against the upstream
 * header reach the right routing.
 */
#define RUNCOMMAND_USER                   (WM_USER + 3000)
/* v1: returns the host's installation directory (the directory
 *     containing the running executable) into a plugin-allocated
 *     wide buffer.
 *     wParam: capacity in TCHARs.
 *     lParam: TCHAR* OUT.
 *     Returns 1 on success, 0 on bad arguments or unresolvable
 *     executable path. */
#define NPPM_GETNPPDIRECTORY              (RUNCOMMAND_USER + 23)
/* v1: returns the full path of the running executable (the
 *     installation directory plus the binary's filename).
 *     Same wParam/lParam contract as NPPM_GETNPPDIRECTORY. */
#define NPPM_GETNPPFULLFILEPATH           (RUNCOMMAND_USER + 42)

/*
 * Selectors for NPPM_GETMENUHANDLE — Notepad++ has historically
 * used these constants for the wParam.
 */
#define NPPPLUGINMENU 0  /* the per-plugin submenu under "Plugins" */
#define NPPMAINMENU   1  /* the entire main menu bar (HMENU) */

/*
 * Selectors for NPPM_GETNBOPENFILES / NPPM_GETOPENFILENAMES wParam.
 * ALL_OPEN_FILES queries the union across views; PRIMARY_VIEW and
 * SECOND_VIEW restrict to one. The numeric values match Notepad++'s
 * public ABI so plugins compiled against either header use the same
 * codes on the wire.
 */
#define ALL_OPEN_FILES 0
#define PRIMARY_VIEW   1
#define SECOND_VIEW    2

/*
 * Encoding (UniMode) values returned by NPPM_GETBUFFERENCODING.
 * Numeric values match Notepad++'s public ABI so plugins compiled
 * against either header use the same wire codes.
 *
 * Each name is a `#define` rather than an `enum` member so a plugin
 * source that also includes Notepad++'s upstream header (the typical
 * port-from-N++ scenario) is safe regardless of which definition
 * style upstream uses. Per-name `#ifndef` guards mean the second
 * header to be included observes the values already defined and
 * skips redefining them — the values agree, so whichever header
 * runs first wins and the other is a no-op.
 *
 *   uni8Bit         (0) - ANSI / system codepage / unknown 8-bit.
 *   uniUTF8         (1) - UTF-8 with BOM (the BOM-prefixed variant).
 *   uniUTF16BE      (2) - UTF-16 big-endian, with BOM.
 *   uniUTF16LE      (3) - UTF-16 little-endian, with BOM.
 *   uniCookie       (4) - UTF-8 without BOM.
 *   uni7Bit         (5) - pure 7-bit ASCII. Code++'s detection
 *                         pipeline reports pure ASCII as `uniCookie`
 *                         (UTF-8 without BOM); this constant exists
 *                         for ABI completeness and is never the
 *                         return value of NPPM_GETBUFFERENCODING.
 *   uniUTF16BE_NoBOM(6) - UTF-16 BE, no BOM (heuristic-detected).
 *   uniUTF16LE_NoBOM(7) - UTF-16 LE, no BOM (heuristic-detected).
 *   uniEnd          (8) - sentinel; never returned.
 */
#ifndef uni8Bit
#define uni8Bit          0
#endif
#ifndef uniUTF8
#define uniUTF8          1
#endif
#ifndef uniUTF16BE
#define uniUTF16BE       2
#endif
#ifndef uniUTF16LE
#define uniUTF16LE       3
#endif
#ifndef uniCookie
#define uniCookie        4
#endif
#ifndef uni7Bit
#define uni7Bit          5
#endif
#ifndef uniUTF16BE_NoBOM
#define uniUTF16BE_NoBOM 6
#endif
#ifndef uniUTF16LE_NoBOM
#define uniUTF16LE_NoBOM 7
#endif
#ifndef uniEnd
#define uniEnd           8
#endif

/* Type alias for `UniMode`. `int` (rather than `enum UniMode_`) so
 * plugins that include both this header and upstream N++ don't see
 * conflicting tag declarations. Numeric values are stable per the
 * ABI; an `int` here is wide enough and unambiguous. */
#ifndef NPP_UNIMODE_TYPEDEF
#define NPP_UNIMODE_TYPEDEF
typedef int UniMode;
#endif

/*
 * EOL format (EolType) values returned by NPPM_GETBUFFERFORMAT.
 * Numeric values match Notepad++'s public ABI. Same per-name
 * `#ifndef` guard rationale as UniMode above.
 *
 *   WIN_FORMAT  (0) - CRLF (Windows / DOS / HTTP / most net protocols).
 *   MAC_FORMAT  (1) - CR (pre-OS X Macintosh).
 *   UNIX_FORMAT (2) - LF (Unix / Linux / modern macOS).
 */
#ifndef WIN_FORMAT
#define WIN_FORMAT  0
#endif
#ifndef MAC_FORMAT
#define MAC_FORMAT  1
#endif
#ifndef UNIX_FORMAT
#define UNIX_FORMAT 2
#endif

#ifndef NPP_EOLTYPE_TYPEDEF
#define NPP_EOLTYPE_TYPEDEF
typedef int EolType;
#endif

/*
 * Lang-type IDs returned by NPPM_GETCURRENTLANGTYPE etc. These match
 * Notepad++'s `LangType` enum in numeric order. Phase 3 ships the
 * txt/cpp/rust/python entries; the rest land with Phase 4's lexer
 * wiring.
 */
typedef enum LangType_ {
    L_TEXT = 0,
    L_PHP, L_C, L_CPP, L_CS, L_OBJC, L_JAVA, L_RC, L_HTML, L_XML,
    L_MAKEFILE, L_PASCAL, L_BATCH, L_INI, L_ASCII, L_USER, L_ASP,
    L_SQL, L_VB, L_JS, L_CSS, L_PERL, L_PYTHON, L_LUA, L_TEX,
    L_FORTRAN, L_BASH, L_FLASH, L_NSIS, L_TCL, L_LISP, L_SCHEME,
    L_ASM, L_DIFF, L_PROPS, L_PS, L_RUBY, L_SMALLTALK, L_VHDL,
    L_KIX, L_AU3, L_CAML, L_ADA, L_VERILOG, L_MATLAB, L_HASKELL,
    L_INNO, L_SEARCHRESULT, L_CMAKE, L_YAML, L_COBOL, L_GUI4CLI,
    L_D, L_POWERSHELL, L_R, L_JSP, L_COFFEESCRIPT, L_JSON,
    L_JAVASCRIPT, L_FORTRAN_77, L_BAANC, L_SREC, L_IHEX, L_TEHEX,
    L_SWIFT, L_ASN1, L_AVS, L_BLITZBASIC, L_PUREBASIC, L_FREEBASIC,
    L_CSOUND, L_ERLANG, L_ESCRIPT, L_FORTH, L_LATEX, L_MMIXAL,
    L_NIM, L_NNCRONTAB, L_OSCRIPT, L_REBOL, L_REGISTRY, L_RUST,
    L_SPICE, L_TXT2TAGS, L_VISUALPROLOG, L_TYPESCRIPT, L_GDSCRIPT,
    L_HOLLYWOOD, L_GOLANG, L_RAKU, L_TOML, L_SAS, L_ERRORLIST,
    L_EXTERNAL,
    /* JSON5 — distinct from L_JSON so plugins can address either
       independently via NPPM_SETBUFFERLANGTYPE. Same lexer
       backing (LexJSON parses both flavours). */
    L_JSON5
} LangType;

/* ------------------------------------------------------------------ */
/* NPPN_* — notification codes (Code++ -> plugin via beNotified())     */
/* ------------------------------------------------------------------ */

/* v1: plugin has been initialized; setInfo/getFuncsArray have run. */
#define NPPN_READY                 (NPPN_FIRST + 1)
/* v1: toolbar-button registration window. */
#define NPPN_TBMODIFICATION        (NPPN_FIRST + 2)
#define NPPN_FILEBEFORECLOSE       (NPPN_FIRST + 3)
/* v1: a file was opened. */
#define NPPN_FILEOPENED            (NPPN_FIRST + 4)
/* v1: a file was closed. */
#define NPPN_FILECLOSED            (NPPN_FIRST + 5)
/* v2: fired before the host begins reading a file from disk.
 *     Code++'s notifications are queue-deferred — by the time the
 *     plugin runs, the load is already in flight — but the queue
 *     ordering matches "BEFORE_OPEN before FILEOPENED" which is
 *     the contract plugins gate on. Carries no buffer id (the tab
 *     hasn't been allocated yet); N++ uses the same convention. */
#define NPPN_FILEBEFOREOPEN        (NPPN_FIRST + 6)
/* v2: fired before the host writes a buffer to disk. Pairs with
 *     NPPN_FILESAVED so plugins observing the buffer's pre-save
 *     state on BEFORE_SAVE can compare against the post-save
 *     observation on FILESAVED. */
#define NPPN_FILEBEFORESAVE        (NPPN_FIRST + 7)
/* v1: a file was saved. */
#define NPPN_FILESAVED             (NPPN_FIRST + 8)
/* v1: Code++ is shutting down; plugin should clean up. */
#define NPPN_SHUTDOWN              (NPPN_FIRST + 9)
/* v1: a different buffer became the active one. */
#define NPPN_BUFFERACTIVATED       (NPPN_FIRST + 10)
/* v1: the active buffer's lang-type changed. */
#define NPPN_LANGCHANGED           (NPPN_FIRST + 11)

#define NPPN_WORDSTYLESUPDATED     (NPPN_FIRST + 12)
/* v3: a plugin's command-shortcut binding was changed.
 *     `nmhdr.idFrom` is the cmd id whose binding moved.
 *     `nmhdr.hwndFrom` carries a `ShortcutKey*` for an
 *     add/remap, NULL for a removal. Code++ today only
 *     fires this on the removal path (`NPPM_REMOVESHORTCUTBYCMDID`
 *     success); the add/remap path lands with
 *     `NPPM_SETSHORTCUTBYCMDID` in Phase 5. Plugins watching
 *     their own bindings see "the user removed your
 *     shortcut" by matching cmd id + NULL hwndFrom. */
#define NPPN_SHORTCUTREMAPPED      (NPPN_FIRST + 13)
#define NPPN_FILEBEFORELOAD        (NPPN_FIRST + 14)
/* v3: a previously-issued open did not complete successfully —
 *     load worker reported an error or the file was missing /
 *     unreadable. Pairs with `NPPN_FILEBEFOREOPEN` (every BEFORE
 *     gets exactly one of FILEOPENED / FILELOADFAILED). Carries
 *     a buffer id of 0 — the load failed before a tab id was
 *     assigned. */
#define NPPN_FILELOADFAILED        (NPPN_FIRST + 15)
#define NPPN_READONLYCHANGED       (NPPN_FIRST + 16)
/* v3: tab order changed via the user's drag-to-reorder gesture
 *     or via `NPPM_*` paths that route through `Shell::move_tab`.
 *     Plugins that maintain per-tab UI state can re-sync from
 *     `NPPM_GETOPENFILENAMES`. No per-tab id — N++'s ABI fires
 *     this as a global "the list shape changed" event.
 *     No-op moves (drag-and-drop back to the same slot) are
 *     filtered out by the host — plugins only see real
 *     reorders. */
#define NPPN_DOCORDERCHANGED       (NPPN_FIRST + 17)
/* v3: a session-restored buffer was rehydrated from its dirty
 *     backup file (untitled `new N` or a saved file whose
 *     last-edit was unsaved). Fires once per restored tab,
 *     after the buffer text is populated and the tab's metadata
 *     is in place. Carries the freshly-allocated buffer id.
 *     Plugins that audit-log file activity treat this like a
 *     `FILEOPENED` for the "what happened to last session's
 *     unsaved work" path. */
#define NPPN_SNAPSHOTDIRTYFILELOADED (NPPN_FIRST + 18)
/* v3: app is about to commit to shutdown — fired before
 *     NPPN_SHUTDOWN so plugins can save their own state
 *     alongside the host's session save. Upstream allows
 *     plugins to *veto* shutdown by responding to this in
 *     `beNotified`; Code++'s queue-deferred delivery means
 *     a veto cannot stop teardown that is already in
 *     progress, so the notification is informational only.
 *     Carries no buffer id; `nmhdr.idFrom` is 0. */
#define NPPN_BEFORESHUTDOWN        (NPPN_FIRST + 19)
#define NPPN_CANCELSHUTDOWN        (NPPN_FIRST + 20)
#define NPPN_FILEBEFORERENAME      (NPPN_FIRST + 21)
#define NPPN_FILERENAMECANCEL      (NPPN_FIRST + 22)
#define NPPN_FILERENAMED           (NPPN_FIRST + 23)
#define NPPN_FILEBEFOREDELETE      (NPPN_FIRST + 24)
#define NPPN_FILEDELETEFAILED      (NPPN_FIRST + 25)
/* v3: a file open in a tab was deleted (or moved out from
 *     under us) externally — by another process, the OS file
 *     manager, a sync client, etc. Code++'s file watcher
 *     reports the `FileChange::Removed` event; the host queues
 *     this notification when the path matches a currently-open
 *     tab. The buffer text stays in memory (the tab is NOT
 *     auto-closed; the user can save-as to recover the file).
 *     Watcher stragglers — Removed events for paths no longer
 *     in any tab — are silently dropped, so plugins only see
 *     the notification for files they could have observed open.
 *     `nmhdr.idFrom` carries the deleted file's buffer id. */
#define NPPN_FILEDELETED           (NPPN_FIRST + 26)
/* v3: the OS-level dark/light theme preference changed.
 *     On Windows the host detects this via `WM_SETTINGCHANGE`
 *     with `lparam` pointing at the wide string
 *     `"ImmersiveColorSet"`. Plugins that paint their own
 *     chrome (a docked panel, a custom dialog, themed
 *     tooltips) re-read the system colour state and repaint
 *     when they observe this notification. Code++ does not
 *     yet ship its own dark-mode rendering — the
 *     notification is forwarded as informational only so
 *     dark-mode-aware plugins can light up without the host
 *     holding them up. Carries no payload; `nmhdr.idFrom`
 *     is 0. */
#define NPPN_DARKMODECHANGED       (NPPN_FIRST + 27)
#define NPPN_CMDLINEPLUGINMSG      (NPPN_FIRST + 28)
#define NPPN_EXTERNALLEXERBUFFER   (NPPN_FIRST + 29)
#define NPPN_GLOBALMODIFIED        (NPPN_FIRST + 30)

#endif /* CODEPP_NPPCOMPAT_NOTEPAD_PLUS_MSGS_H */
