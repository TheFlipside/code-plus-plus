/*
 * PluginInterface.h — Notepad++-compatible plugin entry-point ABI.
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
 */

#ifndef CODEPP_NPPCOMPAT_PLUGININTERFACE_H
#define CODEPP_NPPCOMPAT_PLUGININTERFACE_H

/*
 * A plugin DLL exports exactly six C entry points that Code++ resolves
 * at load time. The Win32 calling convention is __cdecl on x86 and
 * x64 (__cdecl is a no-op on x64; this matches what Notepad++ does and
 * keeps every existing plugin DLL ABI-compatible).
 *
 * Plugins are responsible for #defining UNICODE / _UNICODE before
 * including this header, so TCHAR resolves to wchar_t. Code++ on
 * Windows passes wide strings throughout; an ANSI plugin (isUnicode()
 * returning FALSE) is supported in principle but not exercised by
 * Phase 3 — Code++ ships no ANSI conversion shims.
 */

#include <windows.h>
#include "Scintilla.h"            /* SCNotification, sptr_t, uptr_t */
#include "Notepad_plus_msgs.h"    /* NPPM_*, NPPN_* */

#ifdef __cplusplus
#  include <cstdbool>             /* bool in extern "C" structs (C++17+) */
extern "C" {
#else
#  include <stdbool.h>            /* bool in pure C builds */
#endif

/*
 * Convenience macro for plugin authors. Apply at each entry-point
 * **definition** so the function exports with C linkage regardless of
 * whether the .cpp / .c file wraps it in `extern "C"`. Not applying
 * this (or hand-writing `extern "C" __declspec(dllexport)`) on the
 * definition produces a name-mangled symbol that GetProcAddress
 * cannot resolve — Code++ then refuses to load the DLL with a
 * "missing entry point" error.
 *
 * Example:
 *
 *   CODEPP_PLUGIN_EXPORT void setInfo(NppData data) { ... }
 */
#ifdef __cplusplus
#  define CODEPP_PLUGIN_EXPORT extern "C" __declspec(dllexport)
#else
#  define CODEPP_PLUGIN_EXPORT __declspec(dllexport)
#endif

/*
 * Maximum number of TCHARs in a FuncItem's menu-item name, **including**
 * the trailing null. Notepad++ has historically capped this at 64.
 */
#define MENU_TITLE_LENGTH 64

/*
 * Host data delivered to the plugin in the first setInfo() call.
 *
 *   _nppHandle             — the main Code++ window. Plugins SendMessage
 *                            this with NPPM_* requests.
 *   _scintillaMainHandle   — the primary Scintilla view's HWND. Plugins
 *                            SendMessage this with SCI_* requests.
 *   _scintillaSecondHandle — the secondary view's HWND. Code++ Phase 3
 *                            ships single-view; this handle is NULL
 *                            until the split-view feature lands. Well-
 *                            behaved plugins must check for NULL before
 *                            sending it messages.
 */
typedef struct NppData_ {
    HWND _nppHandle;
    HWND _scintillaMainHandle;
    HWND _scintillaSecondHandle;
} NppData;

/*
 * Keyboard accelerator for a plugin menu item. _isCtrl/_isAlt/_isShift
 * are the modifier flags; _key is the virtual-key code (VK_* per the
 * Win32 API). NULL in FuncItem._pShKey means "no accelerator".
 *
 * Field types are `bool` (1 byte each), not Win32 `BOOL` (4 bytes).
 * This matches the public ABI: a plugin compiled against the standard
 * Notepad++ headers passes a 4-byte struct (3 × bool + 1 × UCHAR),
 * not a 16-byte one. Reading `BOOL` here would dereference past the
 * struct and corrupt subsequent parsing of the plugin's accelerator
 * table.
 */
typedef struct ShortcutKey_ {
    bool _isCtrl;
    bool _isAlt;
    bool _isShift;
    UCHAR _key;
} ShortcutKey;

/*
 * Function pointer for a plugin's menu command. Called on the UI
 * thread; takes no parameters and returns nothing.
 */
typedef void (*PFUNCPLUGINCMD)(void);

/*
 * One menu entry contributed by the plugin. The plugin returns an
 * array of these from getFuncsArray(). Each entry becomes a Code++
 * menu item under "Plugins → <plugin name> → <_itemName>".
 *
 *   _itemName       — UTF-16 menu label, null-terminated, ≤ MENU_TITLE_LENGTH.
 *   _pFunc          — invoked when the item is clicked.
 *   _cmdID          — set by Code++ at load time to the menu-command
 *                     identifier; plugins read this back to send
 *                     NPPM_MENUCOMMAND or check menu state. Plugins
 *                     should leave this 0 in the static initializer.
 *   _init2Check     — if TRUE, the menu item starts in the checked state
 *                     (a checkmark glyph). Plugins toggle subsequently
 *                     via NPPM_SETMENUITEMCHECK.
 *   _pShKey         — optional accelerator. Heap-allocated by the plugin
 *                     (typically `new ShortcutKey{...}`); ownership stays
 *                     with the plugin and survives until SHUTDOWN.
 */
typedef struct FuncItem_ {
    TCHAR _itemName[MENU_TITLE_LENGTH];
    PFUNCPLUGINCMD _pFunc;
    int _cmdID;
    BOOL _init2Check;
    ShortcutKey *_pShKey;
} FuncItem;

/*
 * The six required entry points. Plugins implement these in their own
 * translation unit; the dllexport attribute makes them visible to
 * Code++'s GetProcAddress lookups.
 *
 * Lifecycle (Code++ Phase 3, matching Notepad++):
 *
 *   1. Code++ enumerates plugin DLLs in the plugins folder; each DLL
 *      stays unloaded.
 *   2. On first user touch (Plugins menu open, hotkey, etc.) the DLL
 *      is mapped, the six entry points are resolved, then:
 *        a) setInfo(NppData) — host hands over its window handles.
 *        b) getName() — returns the menu-bar label for this plugin.
 *        c) getFuncsArray() — returns the menu entries.
 *      Code++ then installs the menu items, assigns each a _cmdID,
 *      and fires NPPN_READY.
 *   3. beNotified() is called for every NPPN_ / SCN_ notification
 *      delivered while the plugin is loaded.
 *   4. messageProc() is called for plugin-targeted Win32 messages
 *      that aren't NPPN/SCN notifications.
 *   5. NPPN_SHUTDOWN fires before the DLL is unloaded.
 *
 * Plugins must not perform expensive work in setInfo or getName —
 * those run synchronously on the UI thread.
 */

__declspec(dllexport) void         setInfo(NppData);
__declspec(dllexport) const TCHAR *getName(void);
__declspec(dllexport) FuncItem    *getFuncsArray(int *nbF);
__declspec(dllexport) void         beNotified(SCNotification *notification);
__declspec(dllexport) LRESULT      messageProc(UINT message, WPARAM wParam, LPARAM lParam);
__declspec(dllexport) BOOL         isUnicode(void);

#ifdef __cplusplus
}
#endif

#endif /* CODEPP_NPPCOMPAT_PLUGININTERFACE_H */
