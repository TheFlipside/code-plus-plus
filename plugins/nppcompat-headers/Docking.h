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
 * the value (e.g. UPDATEDISPINFO refreshes the frame title from
 * pszName) — the plugin must keep the buffer alive for the lifetime
 * of the registration.
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
 * Code++ floating-only mode (Phase 4 m4): hClient, pszName, dlgID,
 * uMask, hIconTab, pszModuleName are honoured. rcFloat is honoured
 * if non-empty (used as the floating frame's initial position);
 * empty rcFloat falls back to a default offset from the host
 * window. iPrevCont and pszAddInfo are stored but not yet rendered
 * — the docking title bar lands in Phase 5 with the rest of the
 * cross-platform dock UX. DWS_DF_CONT_* flags are likewise stored
 * but the host always opens the dialog floating until Phase 5.
 */
typedef struct tTbData_ {
    HWND        hClient;        /* plugin's docking-dialog HWND */
    const TCHAR *pszName;       /* display title (also the lookup name) */
    int         dlgID;          /* plugin's own dialog id (see DMN_* below) */
    UINT        uMask;          /* DWS_* flags (see below) */
    HICON       hIconTab;       /* optional title-bar icon (NULL if none) */
    const TCHAR *pszAddInfo;    /* extra info shown in the title bar */
    RECT        rcFloat;        /* preferred floating position */
    int         iPrevCont;      /* previous container id (CONT_*) */
    const TCHAR *pszModuleName; /* plugin DLL filename without extension */
} tTbData;

/* Container ids, used in iPrevCont and packed into DWS_DF_CONT_* */
#define CONT_LEFT   0
#define CONT_RIGHT  1
#define CONT_TOP    2
#define CONT_BOTTOM 3

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
#define DWS_USEOWNDARKMODE  0x01000000  /* honour plugin's own dark-mode rendering */

#define DWS_DF_CONT_LEFT    (CONT_LEFT   << 28)  /* 0x00000000 */
#define DWS_DF_CONT_RIGHT   (CONT_RIGHT  << 28)  /* 0x10000000 */
#define DWS_DF_CONT_TOP     (CONT_TOP    << 28)  /* 0x20000000 */
#define DWS_DF_CONT_BOTTOM  (CONT_BOTTOM << 28)  /* 0x30000000 */
#define DWS_DF_FLOATING     0x80000000           /* open floating */

/* DMN_* — notifications about a docked dialog.
 *
 * These do NOT go through beNotified. The host sends DMN_CLOSE as an
 * ordinary WM_NOTIFY to the plugin's own hClient window procedure:
 *
 *   wParam       0
 *   lParam       NMHDR*
 *   nmhdr.code   DMN_CLOSE
 *   nmhdr.hwndFrom  the host's frame window (NOT the main window)
 *   nmhdr.idFrom    0
 *
 * That is Notepad++'s shape, field for field, so a plugin written
 * against the upstream headers needs no change. Note in particular
 * that idFrom is 0 rather than tTbData.dlgID: a plugin receives the
 * notification on the very window it registered, so there is nothing
 * to disambiguate.
 *
 * DMN_CLOSE fires when the user closes the dialog — in Code++'s
 * floating-only mode, the frame's close button. The frame is hidden,
 * never destroyed: hClient stays alive and a later NPPM_DMMSHOW
 * re-shows it, so a plugin should treat DMN_CLOSE as "the user hid
 * me" and update its own menu state, not as a teardown signal.
 * Upstream sends it before hiding, and so does Code++.
 *
 * DMN_DOCK / DMN_FLOAT are reserved for the Phase 5 docking-manager
 * bring-up and are never sent in floating-only mode.
 */
#define DMN_FIRST 0x1000
#define DMN_CLOSE (DMN_FIRST + 1)  /* user closed the dialog (frame hidden) */
#define DMN_DOCK  (DMN_FIRST + 2)  /* dialog moved from floating to docked */
#define DMN_FLOAT (DMN_FIRST + 3)  /* dialog moved from docked to floating */

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* CODEPP_NPPCOMPAT_DOCKING_H */
