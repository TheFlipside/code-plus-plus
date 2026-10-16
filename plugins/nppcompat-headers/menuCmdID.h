/*
 * menuCmdID.h — Notepad++-compatible menu-command identifiers.
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
 * Plugins reference these identifiers when they want to invoke a
 * built-in menu command via NPPM_MENUCOMMAND. The numeric values are
 * the public ABI; Code++ maps each to its corresponding internal
 * action so existing plugins that say "Save All" or "Find In Files"
 * via NPPM_MENUCOMMAND with these IDs continue to work.
 *
 * Phase 3 ships a minimal subset; the full set lands progressively.
 * Each #define is annotated with the v-tag indicating which Code++
 * phase the action becomes wired-up. Plugins compile against any
 * constant in this header — invoking an unwired ID simply no-ops at
 * runtime with a tracing warning.
 *
 * Every value is Notepad++'s, name for name: tools/npp-abi-check
 * compares them with Notepad++'s published numbers. Until Phase 5 many
 * were not, some sitting on other Notepad++ commands (DESIGN.md §7.4).
 */

#ifndef CODEPP_NPPCOMPAT_MENUCMDID_H
#define CODEPP_NPPCOMPAT_MENUCMDID_H

/*
 * Base id for built-in menu commands, Notepad++'s IDM. Plugin-allocated
 * cmdIDs (via NPPM_ALLOCATECMDID) come from a non-overlapping range;
 * see Notepad_plus_msgs.h.
 */
#define IDM 40000

/* File menu (v1: Phase 2 already supports Save / Exit; Phase 3 adds
 * the rest). */
#define IDM_FILE       (IDM + 1000)
#define IDM_FILE_NEW              (IDM_FILE + 1)
#define IDM_FILE_OPEN             (IDM_FILE + 2)
#define IDM_FILE_CLOSE            (IDM_FILE + 3)
#define IDM_FILE_CLOSEALL         (IDM_FILE + 4)
#define IDM_FILE_CLOSEALL_BUT_CURRENT (IDM_FILE + 5)
#define IDM_FILE_SAVE             (IDM_FILE + 6)
#define IDM_FILE_SAVEALL          (IDM_FILE + 7)
#define IDM_FILE_SAVEAS           (IDM_FILE + 8)
#define IDM_FILE_PRINT            (IDM_FILE + 10)
/* Print without the dialog. Notepad++ numbers it outside the File
 * range. */
#define IDM_FILE_PRINTNOW         1001
#define IDM_FILE_EXIT             (IDM_FILE + 11)
#define IDM_FILE_LOADSESSION      (IDM_FILE + 12)
#define IDM_FILE_SAVESESSION      (IDM_FILE + 13)
#define IDM_FILE_RELOAD           (IDM_FILE + 14)
#define IDM_FILE_SAVECOPYAS       (IDM_FILE + 15)
#define IDM_FILE_DELETE           (IDM_FILE + 16)
#define IDM_FILE_RENAME           (IDM_FILE + 17)

/* Edit menu (v3+). Code++ wires Cut/Copy/Paste in Phase 3. */
#define IDM_EDIT       (IDM + 2000)
#define IDM_EDIT_CUT              (IDM_EDIT + 1)
#define IDM_EDIT_COPY             (IDM_EDIT + 2)
#define IDM_EDIT_UNDO             (IDM_EDIT + 3)
#define IDM_EDIT_REDO             (IDM_EDIT + 4)
#define IDM_EDIT_PASTE            (IDM_EDIT + 5)
#define IDM_EDIT_DELETE           (IDM_EDIT + 6)
#define IDM_EDIT_SELECTALL        (IDM_EDIT + 7)
#define IDM_EDIT_DUP_LINE         (IDM_EDIT + 10)
#define IDM_EDIT_TRANSPOSE_LINE   (IDM_EDIT + 11)
#define IDM_EDIT_SPLIT_LINES      (IDM_EDIT + 12)
#define IDM_EDIT_JOIN_LINES       (IDM_EDIT + 13)

/* Search menu (v4: Find/Replace + find-in-files). */
#define IDM_SEARCH     (IDM + 3000)
#define IDM_SEARCH_FIND           (IDM_SEARCH + 1)
#define IDM_SEARCH_FINDNEXT       (IDM_SEARCH + 2)
#define IDM_SEARCH_REPLACE        (IDM_SEARCH + 3)
#define IDM_SEARCH_GOTOLINE       (IDM_SEARCH + 4)
#define IDM_SEARCH_TOGGLE_BOOKMARK (IDM_SEARCH + 5)
#define IDM_SEARCH_NEXT_BOOKMARK  (IDM_SEARCH + 6)
#define IDM_SEARCH_PREV_BOOKMARK  (IDM_SEARCH + 7)
#define IDM_SEARCH_CLEAR_BOOKMARKS (IDM_SEARCH + 8)
#define IDM_SEARCH_FINDINFILES    (IDM_SEARCH + 13)

/* View menu (v3+: tab strip; v4: zoom / wrap). */
#define IDM_VIEW       (IDM + 4000)
#define IDM_VIEW_FOLDALL          (IDM_VIEW + 10)
#define IDM_VIEW_WRAP             (IDM_VIEW + 22)
#define IDM_VIEW_ZOOMIN           (IDM_VIEW + 23)
#define IDM_VIEW_ZOOMOUT          (IDM_VIEW + 24)
#define IDM_VIEW_UNFOLDALL        (IDM_VIEW + 29)
#define IDM_VIEW_FULLSCREENTOGGLE (IDM_VIEW + 32)
#define IDM_VIEW_ZOOMRESTORE      (IDM_VIEW + 33)
#define IDM_VIEW_TAB_NEXT         (IDM_VIEW + 95)
#define IDM_VIEW_TAB_PREV         (IDM_VIEW + 96)

/* Encoding menu (v4). The "Encode in" commands reinterpret the buffer
 * and the "Convert to" (CONV2) ones re-encode it. Mind the names:
 * IDM_FORMAT_UTF_8 is UTF-8 *with* the byte-order mark and
 * IDM_FORMAT_AS_UTF_8 is UTF-8 without it. */
#define IDM_FORMAT     (IDM + 5000)
#define IDM_FORMAT_TODOS          (IDM_FORMAT + 1)
#define IDM_FORMAT_TOUNIX         (IDM_FORMAT + 2)
#define IDM_FORMAT_TOMAC          (IDM_FORMAT + 3)
#define IDM_FORMAT_ANSI           (IDM_FORMAT + 4)
#define IDM_FORMAT_UTF_8          (IDM_FORMAT + 5)
#define IDM_FORMAT_UTF_16BE       (IDM_FORMAT + 6)
#define IDM_FORMAT_UTF_16LE       (IDM_FORMAT + 7)
#define IDM_FORMAT_AS_UTF_8       (IDM_FORMAT + 8)
#define IDM_FORMAT_CONV2_ANSI     (IDM_FORMAT + 9)
#define IDM_FORMAT_CONV2_AS_UTF_8 (IDM_FORMAT + 10)
#define IDM_FORMAT_CONV2_UTF_8    (IDM_FORMAT + 11)
#define IDM_FORMAT_CONV2_UTF_16BE (IDM_FORMAT + 12)
#define IDM_FORMAT_CONV2_UTF_16LE (IDM_FORMAT + 13)

/* Language menu (v4.6). IDM_LANG_USER is the base the User Defined
 * Language rows count from, which NPPM_GETNBUSERLANG also hands out:
 * NPPM_MENUCOMMAND with IDM_LANG_USER + 1 + i switches the current
 * buffer to the i-th of them, while the command stays below
 * IDM_LANG_USER_LIMIT. */
#define IDM_LANG       (IDM + 6000)
#define IDM_LANG_USER             (IDM_LANG + 180)
#define IDM_LANG_USER_LIMIT       (IDM_LANG + 210)

/* "?" menu. */
#define IDM_ABOUT      (IDM + 7000)

/* Settings menu (v4+). */
#define IDM_SETTING    (IDM + 8000)

/* Tools menu. */
#define IDM_TOOL       (IDM + 8500)

/* Run menu. */
#define IDM_EXECUTE    (IDM + 9000)

#endif /* CODEPP_NPPCOMPAT_MENUCMDID_H */
