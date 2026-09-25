/*
 * Docking.h — Notepad++-compatible plugin docking-manager ABI
 *
 * Part of Code++ (https://git.fiedler.live/tux/code-plus-plus).
 *
 * This header is an independent reimplementation of the Notepad++ plugin
 * ABI. No source has been copied from Notepad++ or its plugin SDK. The
 * ABI surface defined here — message numbers, struct layouts, function
 * signatures, and behavior contracts — is not protected by copyright;
 * the original header source is, and is therefore not used.
 *
 * Code++ is licensed under the MIT License. See LICENSE at the
 * repository root for the full text.
 *
 * Copyright (c) 2026 Max Fiedler and Code++ contributors.
 */

#ifndef CODEPP_NPPCOMPAT_DOCKING_H
#define CODEPP_NPPCOMPAT_DOCKING_H

#include <windows.h>

#ifdef __cplusplus
extern "C" {
#endif

/* tTbData — payload for NPPM_DMMREGASDCKDLG -------------------------
 *
 * The plugin populates this struct (typically as a static / member)
 * and passes a pointer in lParam. The host reads each field; ownership
 * of every pointer-typed field stays with the plugin. The host reads
 * pszName / pszAddInfo / pszModuleName at every call that depends on
 * the value (e.g. UPDATEDISPINFO re-reads pszName as the name the
 * panel is looked up by) — the plugin must keep the buffer alive for
 * the lifetime of the registration.
 *
 * Layout (x64): 72 bytes.
 *   offset  0  HWND          hClient
 *   offset  8  const TCHAR*  pszName
 *   offset 16  int           dlgID
 *   offset 20  UINT          uMask
 *   offset 24  HICON         hIconTab
 *   offset 32  const TCHAR*  pszAddInfo
 *   offset 40  RECT          rcFloat
 *   offset 56  int           iPrevCont
 *   offset 60  (4 bytes padding to 8-align pszModuleName)
 *   offset 64  const TCHAR*  pszModuleName
 *
 * Layout (x86): 48 bytes — pointers are 4-byte-aligned so no padding
 * is needed.
 *
 * Lifetime: the host retains the tTbData POINTER for as long as the
 * registration lives, because NPPM_DMMUPDATEDISPINFO re-reads it.
 * The struct and every buffer its pszName / pszAddInfo /
 * pszModuleName point at must therefore outlive the registration —
 * a stack temporary is a dangling pointer the moment
 * NPPM_DMMREGASDCKDLG returns. Notepad++ imposes the same contract;
 * a plugin that keeps its tTbData as a member of its dialog object
 * (the usual shape) already satisfies it.
 *
 * pszModuleName must be the plugin's own DLL file name INCLUDING the
 * ".dll" extension — what Notepad++'s own header requires, because it
 * persists that string and uses it to find the plugin again on the
 * next start. dlgID is the index of the plugin's FuncItem that opens
 * this panel, for the same reason: a panel that was open when the host
 * last quit is brought back by the host RUNNING FuncItem[dlgID] at the
 * next start — after NPPN_TBMODIFICATION, before NPPN_READY, and even
 * if the plugin registered the panel itself in between. Both Code++
 * and Notepad++ do this. So dlgID must name a command that shows this
 * panel, and never an unrelated one, which would run on every start.
 * A toggle works when it goes by the plugin's own record of whether
 * the panel is open, starting closed — and when the plugin leaves
 * showing the panel at startup to that run: register it from
 * NPPN_TBMODIFICATION if you like, but a plugin that also shows it
 * there has its toggle close it again a moment later. A plugin that
 * sets either field differently still works, but its panel is not
 * restored. A panel whose plugin is missing at a start — uninstalled,
 * disabled, or failing to load — is not shown and not forgotten: it
 * stays open in the saved layout and comes back, by that same run,
 * the next time the plugin loads. Notepad++ keeps it the same way.
 *
 * Code++ also checks who registered a panel before restoring it that
 * way (Preferences > Security, on by default): it runs the saved
 * command only if the registration came from the plugin pszModuleName
 * names, while Code++ was calling that plugin — its setInfo, a
 * notification, one of its own menu commands, or an NPPM_MSGTOPLUGIN
 * delivered to it. Register your panel from one of those, as nearly
 * every plugin does. A registration sent from a window procedure, a
 * timer or another thread, or one naming another plugin's module,
 * still gets its panel; but at the next start that panel waits, where
 * it was, until the user opens it again.
 *
 * Code++ field support: hClient, pszName, dlgID, uMask,
 * pszModuleName are honoured. A registered panel is an ordinary
 * dock panel — it docks to any side, floats, shares a container
 * with other panels as tabs (the host's own Folder as Workspace
 * and Document Map included), reorders by drag, and is persisted
 * in the host's session file by module and name.
 *
 * uMask's DWS_DF_CONT_* nibble names the container the panel first
 * opens in, which is what upstream means by it: two panels asking
 * for the same one become two tabs of one group rather than two
 * bands. It applies only until the user moves the panel; from then
 * on the remembered position wins.
 *
 * hIconTab is drawn on the panel's tab when uMask carries
 * DWS_ICONTAB; without it the tab carries a generic plugin glyph.
 * rcFloat, iPrevCont and pszAddInfo are stored but not yet acted
 * on: the host decides the floating rectangle itself, and a
 * torn-off panel opens at a third of the main window.
 *
 * On Linux (the GTK backend) the struct is read the same way, but
 * its two handle-typed fields carry toolkit objects:
 *
 *   hClient   a GtkWidget* the plugin created and has not added to
 *             a container or made a window of. The host takes its
 *             own reference (sinking a floating one, as a
 *             container's add does) and puts the widget in a
 *             scrolled container of its own, which is what moves as
 *             the panel docks, floats and tabs. A panel smaller than
 *             the widget's minimum size scrolls rather than painting
 *             over its neighbour. The host shows the widget itself
 *             once, as Notepad++ shows hClient; showing and hiding
 *             the panel after that shows and hides the container,
 *             so ask gtk_widget_is_visible or gtk_widget_get_mapped,
 *             not gtk_widget_get_visible, whether the panel is on
 *             screen. The host never destroys the widget. A plugin
 *             that destroys it ends the registration, and the panel
 *             closes.
 *   hIconTab  a GdkPixbuf*, drawn on the panel's tab under the same
 *             DWS_ICONTAB rule; the host takes its own reference.
 *             Anything that is not a pixbuf gets the generic glyph.
 *
 * NPPM_DMMREGASDCKDLG refuses there a widget that is already in a
 * container (every widget of the host's own is), a toplevel, and
 * anything that is not a widget. Those checks catch mistakes, not
 * malice: a pointer cannot be tested for being a live object
 * without reading it, so pass only a widget you made. The DMN_*
 * notifications cannot be sent to a widget, which has no window
 * procedure; see "On Linux" under DMN_* below.
 *
 * macOS does not host plugin panels yet: NPPM_DMMREGASDCKDLG
 * returns 0 there.
 */
typedef struct tTbData_ {
    HWND        hClient;        /* plugin's docking-dialog HWND */
    const TCHAR *pszName;       /* display title (also the lookup name) */
    int         dlgID;          /* index of the FuncItem that opens this panel */
    UINT        uMask;          /* DWS_* flags (see below) */
    HICON       hIconTab;       /* optional title-bar icon (NULL if none) */
    const TCHAR *pszAddInfo;    /* extra info shown in the title bar */
    RECT        rcFloat;        /* preferred floating position */
    int         iPrevCont;      /* previous container id (CONT_*) */
    const TCHAR *pszModuleName; /* the plugin's DLL file name, e.g. L"MyPlugin.dll" */
} tTbData;

/* Container ids, used in iPrevCont and packed into DWS_DF_CONT_* */
#define CONT_LEFT   0
#define CONT_RIGHT  1
#define CONT_TOP    2
#define CONT_BOTTOM 3
#define DOCKCONT_MAX 4  /* first number a floating container can have */

/* DWS_* — Docking Window Style flags packed into tTbData.uMask.
 *
 * Two disjoint bit ranges:
 *   bits  0..7  — content flags (which extra UI elements the host
 *                 should render). Floating-only mode honours
 *                 DWS_ICONTAB; the rest are stored for future use.
 *   bits 28..31 — default-container nibble. Combined with
 *                 DWS_DF_FLOATING in bit 31; the four-valued
 *                 (CONT_LEFT/RIGHT/TOP/BOTTOM) container id sits in
 *                 bits 28..30.
 */
#define DWS_ICONTAB         0x00000001  /* hIconTab visible on the tab strip */
#define DWS_ICONBAR         0x00000002  /* hIconTab visible in the title bar */
#define DWS_ADDINFO         0x00000004  /* pszAddInfo visible in title bar */
#define DWS_USEOWNDARKMODE  0x00000008  /* plugin renders its own dark mode */

#define DWS_DF_CONT_LEFT    (CONT_LEFT   << 28)  /* 0x00000000 */
#define DWS_DF_CONT_RIGHT   (CONT_RIGHT  << 28)  /* 0x10000000 */
#define DWS_DF_CONT_TOP     (CONT_TOP    << 28)  /* 0x20000000 */
#define DWS_DF_CONT_BOTTOM  (CONT_BOTTOM << 28)  /* 0x30000000 */
#define DWS_DF_FLOATING     0x80000000           /* open floating */

/* DMN_* — notifications about a docked dialog.
 *
 * These do NOT go through beNotified. Each is an ordinary WM_NOTIFY
 * sent to the plugin's own hClient window procedure:
 *
 *   wParam          0
 *   lParam          NMHDR*
 *   nmhdr.code      MAKELONG(DMN_xxx, container)
 *   nmhdr.hwndFrom  the host's MAIN window (the one in NppData)
 *   nmhdr.idFrom    0
 *
 * That is Notepad++'s shape, field for field, measured against
 * Notepad++ 8.9.6 with a probe plugin loaded into both hosts — so a
 * plugin written against the upstream headers needs no change. Two
 * details a plugin relies on without noticing:
 *
 *   - Switch on LOWORD(nmhdr.code). The high word carries the
 *     container number for DMN_DOCK and DMN_FLOAT, so comparing the
 *     whole code fails for every panel docked anywhere but the left.
 *   - hwndFrom is the main window for all three. Notepad++'s
 *     docking-dialog template ignores a WM_NOTIFY whose hwndFrom is
 *     not the main window it was initialised with.
 *
 * DMN_CLOSE fires when the user closes the panel from the X on its
 * group's caption; the high word is 0. The panel is hidden, never
 * destroyed: hClient stays alive, its position is remembered, and a
 * later NPPM_DMMSHOW reopens it there — so a plugin should treat
 * DMN_CLOSE as "the user hid me" and update its own menu state, not
 * as a teardown signal. Sent before the panel hides, as upstream does.
 *
 * DMN_DOCK / DMN_FLOAT tell the plugin which container its panel is
 * in. For DMN_DOCK the high word is the CONT_* value of the side the
 * panel is docked to — the same numbering DWS_DF_CONT_* uses on the
 * way in. For DMN_FLOAT it is a number >= DOCKCONT_MAX identifying
 * the floating window, which carries no meaning a plugin can act on.
 * Sent once when the panel is registered, naming the container it
 * will open in, and again whenever it moves to a different container:
 * docked to floating, floating to docked, one side to another, or one
 * floating window to another. Moving between two panels on the same
 * side is not a container change (a side is one container), and
 * neither is hiding and re-showing a panel — so neither sends
 * anything.
 *
 * One deliberate difference from Notepad++: docking a floating panel
 * back by double-clicking its caption, Notepad++ sends DMN_FLOAT
 * (with the docked side's number) rather than DMN_DOCK. Code++ sends
 * DMN_DOCK whenever the panel ends up docked. And one timing
 * difference, in one case only: a panel registered from INSIDE a
 * DMN_DOCK / DMN_FLOAT handler is told its container after that
 * handler returns, not before its own NPPM_DMMREGASDCKDLG returns.
 * The host queues notices raised during a delivery rather than
 * nesting them, so a plugin cannot drive the host's stack arbitrarily
 * deep; every notice is still delivered, in order.
 *
 * DMN_SWITCHIN, DMN_SWITCHOFF and DMN_FLOATDROPPED are declared for
 * completeness and are NOT sent yet. Notepad++ sends them from the
 * panel's container (not the main window) as tabs are switched and
 * containers are rearranged; a plugin must not depend on them under
 * Code++ today.
 *
 * On Linux, where hClient is a widget, the same notifications go to
 * the plugin's messageProc export instead: message WM_NOTIFY
 * (0x004E), lParam the same NMHDR with the same fields, and wParam
 * the panel's hClient, the only way left to say which of the
 * plugin's panels the notification is about. The return value is
 * ignored. Everything else above holds as written, the ordering and
 * the queueing included.
 */
#define DMN_FIRST        1050
#define DMN_CLOSE        (DMN_FIRST + 1)  /* user closed the panel (panel hidden) */
#define DMN_DOCK         (DMN_FIRST + 2)  /* panel is docked; HIWORD(code) = CONT_* */
#define DMN_FLOAT        (DMN_FIRST + 3)  /* panel is floating; HIWORD(code) >= 4 */
#define DMN_SWITCHIN     (DMN_FIRST + 4)  /* not sent by Code++ yet */
#define DMN_SWITCHOFF    (DMN_FIRST + 5)  /* not sent by Code++ yet */
#define DMN_FLOATDROPPED (DMN_FIRST + 6)  /* not sent by Code++ yet */

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* CODEPP_NPPCOMPAT_DOCKING_H */
