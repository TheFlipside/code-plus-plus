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
/* v2: enable/disable Code++'s modeless-dialog forwarding. */
#define NPPM_MODELESSDIALOG               (NPPMSG + 12)

/* Session ----------------------------------------------------------- */

#define NPPM_GETNBSESSIONFILES            (NPPMSG + 13)
#define NPPM_GETSESSIONFILES              (NPPMSG + 14)
#define NPPM_SAVESESSION                  (NPPMSG + 15)
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

#define NPPM_CREATESCINTILLAHANDLE        (NPPMSG + 20)
#define NPPM_DESTROYSCINTILLAHANDLE       (NPPMSG + 21)  /* deprecated upstream */
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
#define NPPM_ENCODESCI                    (NPPMSG + 26)
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

/* Docking / docked-dialog API (DM = "Docking Manager") ------------- */

#define NPPM_DMMSHOW                      (NPPMSG + 30)
#define NPPM_DMMHIDE                      (NPPMSG + 31)
#define NPPM_DMMUPDATEDISPINFO            (NPPMSG + 32)
#define NPPM_DMMREGASDCKDLG               (NPPMSG + 33)
#define NPPM_LOADSESSION                  (NPPMSG + 34)
#define NPPM_DMMVIEWOTHERTAB              (NPPMSG + 35)

/* File operations -------------------------------------------------- */

/* v1: reload the current buffer from disk. */
#define NPPM_RELOADFILE                   (NPPMSG + 36)
/* v1: switch to the buffer whose path is at lParam. */
#define NPPM_SWITCHTOFILE                 (NPPMSG + 37)
/* v1: save the active buffer. */
#define NPPM_SAVECURRENTFILE              (NPPMSG + 38)
#define NPPM_SAVEALLFILES                 (NPPMSG + 39)

#define NPPM_SETMENUITEMCHECK             (NPPMSG + 40)
#define NPPM_ADDTOOLBARICON               (NPPMSG + 41)
#define NPPM_GETWINDOWSVERSION            (NPPMSG + 42)
#define NPPM_DMMGETPLUGINHWNDBYNAME       (NPPMSG + 43)
#define NPPM_MAKECURRENTBUFFERDIRTY       (NPPMSG + 44)
#define NPPM_GETENABLETHEMETEXTUREFUNC    (NPPMSG + 45)  /* deprecated upstream */

/* v1: returns the path of the per-user plugin config dir (TCHAR* via lParam). */
#define NPPM_GETPLUGINSCONFIGDIR          (NPPMSG + 46)
#define NPPM_MSGTOPLUGIN                  (NPPMSG + 47)
/* v1: invoke the menu command identified by lParam (cmdID). */
#define NPPM_MENUCOMMAND                  (NPPMSG + 48)
#define NPPM_TRIGGERTABBARCONTEXTMENU     (NPPMSG + 49)
/* v1: returns Code++'s self-reported version, encoded high-word major,
 *     low-word minor — chosen to be range-compatible with Notepad++ for
 *     plugin gating that does `if (NPPM_GETNPPVERSION() >= ...)`. */
#define NPPM_GETNPPVERSION                (NPPMSG + 50)

#define NPPM_HIDETABBAR                   (NPPMSG + 51)
#define NPPM_ISTABBARHIDDEN               (NPPMSG + 52)

/* Buffer-id queries ------------------------------------------------ */

#define NPPM_GETPOSFROMBUFFERID           (NPPMSG + 57)
/* v1: lParam (TCHAR*) is filled with the full path of the buffer
 *     whose id is in wParam. */
#define NPPM_GETFULLPATHFROMBUFFERID      (NPPMSG + 58)
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

#define NPPM_HIDETOOLBAR                  (NPPMSG + 70)
#define NPPM_ISTOOLBARHIDDEN              (NPPMSG + 71)
#define NPPM_HIDEMENU                     (NPPMSG + 72)
#define NPPM_ISMENUHIDDEN                 (NPPMSG + 73)
#define NPPM_HIDESTATUSBAR                (NPPMSG + 74)
#define NPPM_ISSTATUSBARHIDDEN            (NPPMSG + 75)

#define NPPM_GETSHORTCUTBYCMDID           (NPPMSG + 76)
/* v1: open the file at lParam (TCHAR*). */
#define NPPM_DOOPEN                       (NPPMSG + 77)
#define NPPM_SAVECURRENTFILEAS            (NPPMSG + 78)
#define NPPM_GETCURRENTNATIVELANGENCODING (NPPMSG + 79)
#define NPPM_ALLOCATESUPPORTED            (NPPMSG + 80)
#define NPPM_ALLOCATECMDID                (NPPMSG + 81)
#define NPPM_ALLOCATEMARKER               (NPPMSG + 82)
#define NPPM_GETLANGUAGENAME              (NPPMSG + 83)
#define NPPM_GETLANGUAGEDESC              (NPPMSG + 84)
#define NPPM_SHOWDOCSWITCHER              (NPPMSG + 85)
#define NPPM_ISDOCSWITCHERSHOWN           (NPPMSG + 86)
#define NPPM_GETAPPDATAPLUGINSALLOWED     (NPPMSG + 87)
#define NPPM_GETCURRENTVIEW               (NPPMSG + 88)
#define NPPM_DOCSWITCHERDISABLECOLUMN     (NPPMSG + 89)
#define NPPM_GETEDITORDEFAULTFOREGROUNDCOLOR (NPPMSG + 90)
#define NPPM_GETEDITORDEFAULTBACKGROUNDCOLOR (NPPMSG + 91)
#define NPPM_SETSMOOTHFONT                (NPPMSG + 92)
#define NPPM_SETEDITORBORDEREDGE          (NPPMSG + 93)
#define NPPM_SAVEFILE                     (NPPMSG + 94)
#define NPPM_DISABLEAUTOUPDATE            (NPPMSG + 95)
#define NPPM_REMOVESHORTCUTBYCMDID        (NPPMSG + 96)
#define NPPM_GETPLUGINHOMEPATH            (NPPMSG + 97)
#define NPPM_GETSETTINGSCLOUDPATH         (NPPMSG + 98)
#define NPPM_SETLINENUMBERWIDTHMODE       (NPPMSG + 99)
#define NPPM_GETLINENUMBERWIDTHMODE       (NPPMSG + 100)
#define NPPM_GETBOOKMARKID                (NPPMSG + 101)
#define NPPM_GETZOOMLEVEL                 (NPPMSG + 102)

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
    L_EXTERNAL
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
#define NPPN_FILEBEFOREOPEN        (NPPN_FIRST + 6)
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
#define NPPN_SHORTCUTREMAPPED      (NPPN_FIRST + 13)
#define NPPN_FILEBEFORELOAD        (NPPN_FIRST + 14)
#define NPPN_FILELOADFAILED        (NPPN_FIRST + 15)
#define NPPN_READONLYCHANGED       (NPPN_FIRST + 16)
#define NPPN_DOCORDERCHANGED       (NPPN_FIRST + 17)
#define NPPN_SNAPSHOTDIRTYFILELOADED (NPPN_FIRST + 18)
#define NPPN_BEFORESHUTDOWN        (NPPN_FIRST + 19)
#define NPPN_CANCELSHUTDOWN        (NPPN_FIRST + 20)
#define NPPN_FILEBEFORERENAME      (NPPN_FIRST + 21)
#define NPPN_FILERENAMECANCEL      (NPPN_FIRST + 22)
#define NPPN_FILERENAMED           (NPPN_FIRST + 23)
#define NPPN_FILEBEFOREDELETE      (NPPN_FIRST + 24)
#define NPPN_FILEDELETEFAILED      (NPPN_FIRST + 25)
#define NPPN_FILEDELETED           (NPPN_FIRST + 26)
#define NPPN_DARKMODECHANGED       (NPPN_FIRST + 27)
#define NPPN_CMDLINEPLUGINMSG      (NPPN_FIRST + 28)
#define NPPN_EXTERNALLEXERBUFFER   (NPPN_FIRST + 29)
#define NPPN_GLOBALMODIFIED        (NPPN_FIRST + 30)

#endif /* CODEPP_NPPCOMPAT_NOTEPAD_PLUS_MSGS_H */
