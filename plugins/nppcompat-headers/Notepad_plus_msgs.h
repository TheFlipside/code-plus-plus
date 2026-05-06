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
/* v2: returns the count of currently open files (both views). */
#define NPPM_GETNBOPENFILES               (NPPMSG + 7)
/* v2: fills lParam (TCHAR**) with full paths of open files. */
#define NPPM_GETOPENFILENAMES             (NPPMSG + 8)
/* v2: enable/disable Code++'s modeless-dialog forwarding. */
#define NPPM_MODELESSDIALOG               (NPPMSG + 12)

/* Session ----------------------------------------------------------- */

#define NPPM_GETNBSESSIONFILES            (NPPMSG + 13)
#define NPPM_GETSESSIONFILES              (NPPMSG + 14)
#define NPPM_SAVESESSION                  (NPPMSG + 15)
#define NPPM_SAVECURRENTSESSION           (NPPMSG + 16)

/* Multi-view (split-view) — primary / secondary --------------------- */

#define NPPM_GETOPENFILENAMESPRIMARY      (NPPMSG + 17)
#define NPPM_GETOPENFILENAMESSECOND       (NPPMSG + 18)

/* Scintilla handle management -------------------------------------- */

#define NPPM_CREATESCINTILLAHANDLE        (NPPMSG + 20)
#define NPPM_DESTROYSCINTILLAHANDLE       (NPPMSG + 21)  /* deprecated upstream */
#define NPPM_GETNBUSERLANG                (NPPMSG + 22)
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
#define NPPM_RELOADBUFFERID               (NPPMSG + 61)
/* v1: returns the lang-type id for the buffer at wParam. */
#define NPPM_GETBUFFERLANGTYPE            (NPPMSG + 64)
#define NPPM_SETBUFFERLANGTYPE            (NPPMSG + 65)
#define NPPM_GETBUFFERENCODING            (NPPMSG + 66)
#define NPPM_SETBUFFERENCODING            (NPPMSG + 67)
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
