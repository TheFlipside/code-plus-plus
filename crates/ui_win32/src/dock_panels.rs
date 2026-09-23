//! Win32 mechanism for the plugin-panel docking subsystem.
//!
//! All policy lives in `codepp_core::dock` (`DockLayout`,
//! `compute_frame`, `resolve_drop`) — this module only supplies what
//! a window system must: the group container windows (caption bar +
//! panel content + bottom tab bar), the four side splitters, the
//! grey drop-hint overlay, mouse capture for caption/tab drags, and
//! the reconciler that makes the native window tree match the model
//! after every mutation.
//!
//! Structure of a group window:
//!
//! ```text
//! ┌──────────────────────────────┐
//! │ caption: active title      ✕ │  DOCK_CAPTION_H — the drag bar
//! ├──────────────────────────────┤
//! │                              │
//! │  active panel's content HWND │  (workspace tree / docmap view,
//! │                              │   reparented in — never owned)
//! ├──────────────────────────────┤
//! │ [icon Label] [icon] …        │  DOCK_TAB_BAR_H — only when the
//! └──────────────────────────────┘  group holds 2+ panels
//! ```
//!
//! **Group windows are recreated, never restyled, on dock ↔ float
//! transitions.** A docked group is a `WS_CHILD` of the main window;
//! a floating one is a `WS_POPUP | WS_THICKFRAME` tool window owned
//! by it (owned → always above the main window, exactly the
//! "always-on-top windows on the main app UI" the feature asks for).
//! Toggling `WS_CHILD` on a live window via `SetWindowLongPtrW` +
//! `SetParent` is documented-but-fragile territory (activation,
//! menu-loop and DWM edge cases); the group container holds no state
//! of its own — identity lives on the model's group id, content in
//! the reparented panel HWNDs — so destroying and recreating the
//! thin shell is the boring, correct choice.
//!
//! **State lookups never go through `GetParent` chains hard-coded to
//! one level.** The panel content windows historically found the
//! main window with `GetParent(panel)`; once panels live inside
//! group containers (and group containers may be popups), the only
//! robust route is [`find_main_hwnd`], which walks `GetParent` —
//! which returns the *owner* for popups, covering the floating case
//! for free — until the window class matches the main class.

use codepp_core::dock::{
    compute_frame, resolve_drop, DockContainer, DockLayout, DockLocation, DockPanel, DockRect,
    DockSide, DragSubject, DropTarget, DropZones, MIN_FLOAT_H, MIN_FLOAT_W,
};
use std::ffi::c_void;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, ClientToScreen, CreateCompatibleDC, DeleteDC, DrawTextW, EndPaint,
    FillRect, GetDC, GetObjectW, GetStockObject, GetSysColorBrush, GetTextExtentPoint32W,
    InvalidateRect, ReleaseDC, SelectObject, SetBkMode, AC_SRC_ALPHA, AC_SRC_OVER, BITMAP,
    BLENDFUNCTION, COLOR_3DFACE, COLOR_WINDOW, DEFAULT_GUI_FONT, DT_CENTER, DT_END_ELLIPSIS,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::NMHDR;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClassNameW, GetClientRect, GetCursorPos,
    GetParent, GetWindowLongPtrW, GetWindowRect, LoadCursorW, RegisterClassExW,
    SetLayeredWindowAttributes, SetParent, SetWindowLongPtrW, SetWindowPos, ShowWindow, CS_HREDRAW,
    CS_VREDRAW, GWLP_USERDATA, HWND_TOP, HWND_TOPMOST, IDC_ARROW, IDC_SIZENS, IDC_SIZEWE,
    LWA_ALPHA, MINMAXINFO, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
    SW_SHOW, WINDOW_EX_STYLE, WM_CAPTURECHANGED, WM_GETMINMAXINFO, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_PAINT, WM_SETCURSOR, WM_SIZE, WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WS_THICKFRAME,
};
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, SendMessageW, WM_NOTIFY};

use crate::{
    dialog_bg_brush, relayout_now, state_from_hwnd, sync_docmap_to_active_tab, toolbar,
    WindowState, ID_VIEW_DOCMAP, ID_VIEW_FOLDER_AS_WORKSPACE, MAIN_CLASS, STATUS_HEIGHT_PX,
};

/// Height of a group's caption bar (the "top mini-window bar" the
/// user drags). Matches the old per-panel header rows it replaces.
pub(crate) const DOCK_CAPTION_H: i32 = 22;
/// Height of the bottom tab bar, shown only when a group holds two
/// or more panels.
pub(crate) const DOCK_TAB_BAR_H: i32 = 26;
/// Width of the caption's close-✕ hit box, pinned to the right edge.
pub(crate) const DOCK_CLOSE_W: i32 = 22;
/// Icon display size on a tab (source PNGs are 24 px; `AlphaBlend`
/// scales, same as the editor tab strip's 20 px save glyph).
pub(crate) const DOCK_TAB_ICON_PX: i32 = 16;
/// Horizontal padding inside a tab, either side of its content.
pub(crate) const DOCK_TAB_PAD: i32 = 8;
/// Gap between a tab's icon and its label (active tabs only).
pub(crate) const DOCK_TAB_ICON_GAP: i32 = 5;
/// Pixels of mouse travel before a pressed caption/tab becomes a
/// drag rather than a click. Same 4-px convention as the tab strip.
const DOCK_DRAG_THRESHOLD: i32 = 4;
/// Alpha of the layered drop-hint window (0-255). Translucent
/// enough to read what is underneath, opaque enough to read as a
/// solid grey preview of the drop.
const DOCK_HINT_ALPHA: u8 = 110;

const DOCK_GROUP_CLASS: PCWSTR = w!("CodePlusPlusDockGroup");
const DOCK_HINT_CLASS: PCWSTR = w!("CodePlusPlusDockHint");
const DOCK_SIDE_SPLITTER_CLASS: PCWSTR = w!("CodePlusPlusDockSideSplitter");

/// One live group container: the model group's id and its window.
/// The id is the identity (§7.4's key-on-ids rule); the HWND is
/// disposable and remade on dock ↔ float transitions.
#[derive(Copy, Clone)]
pub(crate) struct DockGroupWindow {
    pub id: u32,
    pub hwnd: HWND,
    /// True iff this window was created as a floating popup. Kept
    /// here rather than re-derived from `GWL_STYLE` so the
    /// reconciler's "does the window kind still match the model?"
    /// check cannot be confused by transient style bits.
    pub floating: bool,
}

/// In-flight caption/tab gesture on a group window. Lives on
/// `WindowState.dock_drag`; cleared on release, cancel, or capture
/// loss.
pub(crate) struct DockDrag {
    /// What a completed drag will move.
    pub subject: DragSubject,
    /// The group the gesture started on (owns the mouse capture).
    pub group_id: u32,
    /// Screen point of the button-down.
    pub start: (i32, i32),
    /// Cursor offset into the float preview, so the grab point
    /// stays under the cursor while floating.
    pub grab: (i32, i32),
    /// Size of the float preview: [`tear_off_size`] when a docked
    /// group or tab is being torn off, the group's own outer size
    /// when an already-floating group is being moved.
    pub float_size: (i32, i32),
    /// Armed tab index — a press on a tab that ends without
    /// crossing the drag threshold is a tab *switch*.
    pub armed_tab: Option<usize>,
    /// Armed close — a press on the caption ✕ that ends with the
    /// cursor still inside the ✕ hides the active panel.
    pub armed_close: bool,
    /// Crossed the drag threshold: the hint is up and release
    /// commits a move.
    pub started: bool,
    /// Esc was pressed mid-drag (or capture was lost): release
    /// does nothing.
    pub cancelled: bool,
}

/// In-flight side-splitter drag. Mirror of the old per-panel
/// splitter drags, generalized over the four sides.
#[derive(Copy, Clone)]
pub(crate) struct DockSideDrag {
    pub side: DockSide,
    pub start: (i32, i32),
    pub size_at_start: i32,
}

// --- pure geometry (unit-tested at the bottom) -------------------------------

/// The caption's close-✕ rect within a group client `width` px wide.
#[must_use]
pub(crate) fn caption_close_rect(width: i32) -> DockRect {
    DockRect::new(
        (width - DOCK_CLOSE_W).max(0),
        0,
        DOCK_CLOSE_W,
        DOCK_CAPTION_H,
    )
}

/// Per-tab `(x, width)` extents inside the tab bar, given each
/// panel's measured label width and which tab is active. Active
/// tab: pad + icon + gap + label + pad; inactive: pad + icon + pad
/// (icon only — the label appears when the tab activates, per the
/// feature spec).
#[must_use]
pub(crate) fn tab_extents(label_widths: &[i32], active: usize) -> Vec<(i32, i32)> {
    let mut out = Vec::with_capacity(label_widths.len());
    let mut x = 0;
    for (i, lw) in label_widths.iter().enumerate() {
        let w = if i == active {
            DOCK_TAB_PAD + DOCK_TAB_ICON_PX + DOCK_TAB_ICON_GAP + lw.max(&0) + DOCK_TAB_PAD
        } else {
            DOCK_TAB_PAD + DOCK_TAB_ICON_PX + DOCK_TAB_PAD
        };
        out.push((x, w));
        x += w;
    }
    out
}

/// Which tab (if any) a client-space x coordinate lands on.
#[must_use]
pub(crate) fn hit_tab(extents: &[(i32, i32)], x: i32) -> Option<usize> {
    extents.iter().position(|(tx, tw)| x >= *tx && x < tx + tw)
}

/// The content rect (where the active panel's window sits) for a
/// group client of `w`×`h` holding `panels` tabs.
#[must_use]
pub(crate) fn group_content_rect(w: i32, h: i32, panels: usize) -> DockRect {
    let tab_h = if panels > 1 { DOCK_TAB_BAR_H } else { 0 };
    DockRect::new(
        0,
        DOCK_CAPTION_H,
        w.max(0),
        (h - DOCK_CAPTION_H - tab_h).max(0),
    )
}

/// New band size for a splitter drag: the delta's sign depends on
/// which side the band hangs off (dragging a Right band's splitter
/// left *grows* the band).
#[must_use]
pub(crate) fn side_drag_size(side: DockSide, size_at_start: i32, delta: (i32, i32)) -> i32 {
    match side {
        DockSide::Left => size_at_start + delta.0,
        DockSide::Right => size_at_start - delta.0,
        DockSide::Top => size_at_start + delta.1,
        DockSide::Bottom => size_at_start - delta.1,
    }
}

// --- registration + chrome creation ------------------------------------------

/// Register the group / hint / side-splitter classes. Idempotent
/// (`OnceLock`), same pattern as `register_fif_classes`.
pub(crate) unsafe fn register_dock_classes() {
    use std::sync::OnceLock;
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let group = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dock_group_wnd_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: dialog_bg_brush(),
            lpszClassName: DOCK_GROUP_CLASS,
            ..Default::default()
        };
        let _ = RegisterClassExW(&raw const group);
        let hint = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dock_hint_wnd_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            // The grey body is painted by SetLayeredWindowAttributes'
            // uniform alpha over this brush; the proc adds the
            // darker outline.
            hbrBackground: GetSysColorBrush(COLOR_3DFACE),
            lpszClassName: DOCK_HINT_CLASS,
            ..Default::default()
        };
        let _ = RegisterClassExW(&raw const hint);
        let splitter = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dock_side_splitter_wnd_proc),
            hInstance: instance.into(),
            // Fallback cursor only — WM_SETCURSOR picks the right
            // orientation per side at hover time.
            hCursor: LoadCursorW(None, IDC_SIZEWE).unwrap_or_default(),
            hbrBackground: dialog_bg_brush(),
            lpszClassName: DOCK_SIDE_SPLITTER_CLASS,
            ..Default::default()
        };
        let _ = RegisterClassExW(&raw const splitter);
    });
}

/// Create the four (hidden) side splitters and the (hidden) drop
/// hint. Called once from `run()` after the main window exists.
/// Each splitter stores its `DockSide` index in `GWLP_USERDATA`.
pub(crate) unsafe fn create_dock_chrome(main_hwnd: HWND) -> ([HWND; 4], HWND) {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let mut splitters = [HWND::default(); 4];
        for (i, slot) in splitters.iter_mut().enumerate() {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                DOCK_SIDE_SPLITTER_CLASS,
                PCWSTR::null(),
                WS_CHILD,
                0,
                0,
                0,
                0,
                Some(main_hwnd),
                None,
                Some(instance.into()),
                None,
            )
            .unwrap_or_default();
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, i as isize);
            *slot = hwnd;
        }
        // The hint is a layered, click-through, never-activated
        // popup: it must not steal the capture from the dragging
        // group window, and it must not flash into the Alt+Tab
        // list. TOPMOST only matters for the drag's duration (it
        // is hidden otherwise) and keeps the preview above
        // floating groups.
        let hint = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            DOCK_HINT_CLASS,
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            Some(main_hwnd),
            None,
            Some(instance.into()),
            None,
        )
        .unwrap_or_default();
        let _ = SetLayeredWindowAttributes(
            hint,
            windows::Win32::Foundation::COLORREF(0),
            DOCK_HINT_ALPHA,
            LWA_ALPHA,
        );
        (splitters, hint)
    }
}

// --- main-window lookup -------------------------------------------------------

/// Walk up from any dock-subsystem window (panel content, group
/// container, splitter) to the main window. `GetParent` returns the
/// parent for child windows and the **owner** for popups, so one
/// loop covers docked and floating groups alike. Identified by
/// window class rather than by depth so intermediate containers can
/// be added without breaking every state lookup — the exact failure
/// the old one-level `GetParent(hwnd)` pattern had.
pub(crate) unsafe fn find_main_hwnd(start: HWND) -> Option<HWND> {
    unsafe {
        let mut hwnd = start;
        // The tree is 3 deep today (panel → group → main); 16 is
        // "unbounded in practice" without risking a loop on a
        // corrupt parent chain.
        for _ in 0..16 {
            if hwnd.0.is_null() {
                return None;
            }
            let mut buf = [0u16; 64];
            let len = GetClassNameW(hwnd, &mut buf) as usize;
            if len > 0 {
                let name = &buf[..len.min(buf.len())];
                let main = MAIN_CLASS.as_wide();
                if name == main {
                    return Some(hwnd);
                }
            }
            hwnd = GetParent(hwnd).unwrap_or_default();
        }
        None
    }
}

// --- reconciler ----------------------------------------------------------------

/// The panel's content window (created once in `run()`, reparented
/// between the main window and group containers, never destroyed).
fn panel_content_hwnd(state: &WindowState, panel: DockPanel) -> HWND {
    match panel {
        DockPanel::Workspace => state.workspace_hwnd,
        DockPanel::DocMap => state.docmap_hwnd,
        // A plugin panel's content is the `h_client` the plugin
        // handed us at `NPPM_DMMREGASDCKDLG`. Null until then: a
        // layout restored from `session.xml` can name a panel whose
        // plugin has not been lazily loaded yet, which is the normal
        // case rather than an error. The reconciler skips a null
        // content window and the group renders as an empty tab until
        // the plugin registers.
        DockPanel::Plugin(_) => state
            .dock_dialogs
            .iter()
            .find(|e| e.panel == panel)
            .map_or(HWND::default(), |e| e.h_client),
    }
}

fn panel_icon_index(panel: DockPanel) -> usize {
    match panel {
        DockPanel::Workspace => 0,
        DockPanel::DocMap => 1,
        // A plugin that supplied its own `tTbData.h_icon_tab` never
        // reaches here — `DockEntry::tab_icon` wins. This is the
        // fallback for one that did not, and it earns a glyph of its
        // own rather than borrowing the document map's: an inactive
        // tab shows the icon and nothing else, so two panels sharing
        // one is two tabs the user cannot tell apart.
        DockPanel::Plugin(_) => 2,
    }
}

/// Make the native window tree match `state.dock_layout`, then
/// relayout and refresh every indicator. The single funnel every
/// mutation goes through — show/hide, drops, tab switches, restore.
///
/// Runs in phases that alternate "borrow state, compute" with
/// "no borrow, call Win32": `CreateWindowExW` / `SetParent` /
/// `DestroyWindow` all deliver messages synchronously to windows
/// whose procs reach for `state_from_hwnd`, so holding the borrow
/// across them is the same aliasing hazard every snapshot-then-drop
/// site in `lib.rs` documents.
pub(crate) unsafe fn apply_dock_layout(main_hwnd: HWND) {
    unsafe {
        // Phase 1 (borrow): snapshot the model and current windows.
        let Some((layout, existing, panel_pair, splitters)) =
            state_from_hwnd(main_hwnd).map(|state| {
                (
                    state.dock_layout.clone(),
                    state.dock_groups.clone(),
                    {
                        // Every panel whose content this reconciler
                        // owns: the two built-in ones, plus each
                        // plugin panel's `h_client`. It was a fixed
                        // pair until plugin panels became peers —
                        // and a plugin panel missing from this list
                        // is not a compile error, it is a group that
                        // appears with nothing inside it.
                        let mut pairs = vec![
                            (DockPanel::Workspace, state.workspace_hwnd),
                            (DockPanel::DocMap, state.docmap_hwnd),
                        ];
                        pairs.extend(state.dock_dialogs.iter().map(|e| (e.panel, e.h_client)));
                        pairs
                    },
                    state.dock_splitters,
                )
            })
        else {
            return;
        };

        // Phase 2 (no borrow): destroy stale group windows, create
        // missing ones.
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let mut survivors: Vec<DockGroupWindow> = Vec::new();
        for gw in &existing {
            let keep = layout
                .group(gw.id)
                .is_some_and(|g| gw.floating == matches!(g.location, DockLocation::Floating(_)));
            if keep {
                survivors.push(*gw);
            } else {
                // Evacuate any panel content back under the main
                // window BEFORE the container dies — a destroyed
                // parent would take the content windows with it.
                for (_, panel_hwnd) in &panel_pair {
                    let panel_hwnd = *panel_hwnd;
                    if GetParent(panel_hwnd).unwrap_or_default() == gw.hwnd {
                        let _ = ShowWindow(panel_hwnd, SW_HIDE);
                        let _ = SetParent(panel_hwnd, Some(main_hwnd));
                    }
                }
                let _ = DestroyWindow(gw.hwnd);
            }
        }
        for group in layout.groups() {
            if survivors.iter().any(|gw| gw.id == group.id) {
                continue;
            }
            let (hwnd, floating) = match group.location {
                DockLocation::Side(_) => {
                    let hwnd = CreateWindowExW(
                        WINDOW_EX_STYLE::default(),
                        DOCK_GROUP_CLASS,
                        PCWSTR::null(),
                        WS_CHILD | WS_CLIPCHILDREN,
                        0,
                        0,
                        0,
                        0,
                        Some(main_hwnd),
                        None,
                        Some(instance.into()),
                        None,
                    )
                    .unwrap_or_default();
                    (hwnd, false)
                }
                DockLocation::Floating(rect) => {
                    // Owner = main window → stays above it, no
                    // taskbar entry (TOOLWINDOW), user-resizable
                    // (THICKFRAME) with our own caption inside.
                    let hwnd = CreateWindowExW(
                        WS_EX_TOOLWINDOW,
                        DOCK_GROUP_CLASS,
                        PCWSTR::null(),
                        WS_POPUP | WS_THICKFRAME | WS_CLIPCHILDREN,
                        rect.x,
                        rect.y,
                        rect.w,
                        rect.h,
                        Some(main_hwnd),
                        None,
                        Some(instance.into()),
                        None,
                    )
                    .unwrap_or_default();
                    (hwnd, true)
                }
            };
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, group.id as isize);
            survivors.push(DockGroupWindow {
                id: group.id,
                hwnd,
                floating,
            });
        }

        // Phase 3 (borrow): publish the reconciled window list.
        if let Some(state) = state_from_hwnd(main_hwnd) {
            state.dock_groups.clone_from(&survivors);
        }

        // Phase 4 (no borrow): parent + show/hide the panel
        // contents, show the right chrome, position everything.
        let group_hwnd = |id: u32| {
            survivors
                .iter()
                .find(|gw| gw.id == id)
                .map(|gw| gw.hwnd)
                .unwrap_or_default()
        };
        for group in layout.groups() {
            let ghwnd = group_hwnd(group.id);
            if ghwnd.0.is_null() {
                continue;
            }
            for (i, panel) in group.panels.iter().enumerate() {
                let Some((_, panel_hwnd)) = panel_pair.iter().find(|(p, _)| p == panel) else {
                    continue;
                };
                if GetParent(*panel_hwnd).unwrap_or_default() != ghwnd {
                    let _ = SetParent(*panel_hwnd, Some(ghwnd));
                }
                let _ = ShowWindow(
                    *panel_hwnd,
                    if i == group.active { SW_SHOW } else { SW_HIDE },
                );
            }
            if let DockLocation::Floating(rect) = group.location {
                let _ = SetWindowPos(
                    ghwnd,
                    Some(HWND_TOP),
                    rect.x,
                    rect.y,
                    rect.w,
                    rect.h,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            } else {
                let _ = ShowWindow(ghwnd, SW_SHOW);
            }
            // Position the content and repaint the chrome
            // explicitly rather than relying on WM_SIZE reaching
            // this group: a torn-off float is *created* at its
            // final rect, so the SetWindowPos above is a same-size
            // no-op that emits no WM_SIZE, and the creation-time
            // WM_SIZE fired before the group id was set and the
            // content was parented in — without this the content
            // keeps its docked extent until the user happens to
            // resize the float. Same story when a panel joins or
            // leaves a group whose frame does not move: the tab
            // bar appears or disappears, the content rect changes,
            // and no WM_SIZE fires. (`on_group_size` also
            // invalidates, covering caption/tab chrome changes.)
            on_group_size(ghwnd);
        }
        // Hidden panels go back under the (invisible) care of the
        // main window so a dying group can never take them along.
        for (panel, panel_hwnd) in panel_pair.iter().copied() {
            if !layout.is_visible(panel) {
                let _ = ShowWindow(panel_hwnd, SW_HIDE);
                if GetParent(panel_hwnd).unwrap_or_default() != main_hwnd {
                    let _ = SetParent(panel_hwnd, Some(main_hwnd));
                }
            }
        }
        for side in DockSide::ALL {
            let occupied = layout.groups_on(side).next().is_some();
            let _ = ShowWindow(
                splitters[side_index(side)],
                if occupied { SW_SHOW } else { SW_HIDE },
            );
        }
        relayout_now(main_hwnd);

        // Freshly shown Document Map needs its doc binding + the
        // viewport box caught up (it skips both while hidden).
        if layout.is_visible(DockPanel::DocMap) {
            sync_docmap_to_active_tab(main_hwnd);
        }
        sync_dock_indicators(main_hwnd);

        // Phase 5 (borrow): write the layout through to the shell's
        // session cache so the next autosave persists it.
        if let Some(state) = state_from_hwnd(main_hwnd) {
            let session = state.dock_layout.to_session();
            state.shell.set_dock_session(Some(session));
        }

        // Phase 6 (borrow, then none): tell each plugin whose panel
        // changed container, and each newly registered one where its
        // panel lives. Last, so a plugin that reacts by querying the
        // host or the window tree finds both already settled — and
        // with no borrow held for the send, so its handler's
        // `NPPM_*` is answered rather than declined.
        let notices = state_from_hwnd(main_hwnd)
            .map(|state| container_notices(&state.dock_layout, &mut state.dock_dialogs))
            .unwrap_or_default();
        deliver_container_notices(main_hwnd, notices);
    }
}

/// Notepad++'s number for a docked container: its `CONT_*` value
/// (`CONT_LEFT` 0, `CONT_RIGHT` 1, `CONT_TOP` 2, `CONT_BOTTOM` 3).
///
/// Written out rather than borrowed from [`side_index`], which
/// happens to agree today but indexes the splitter array and is free
/// to be reordered for that; this one is ABI. A test pins it against
/// the `DWS_DF_CONT_*` nibble decoding, which is the same numbering
/// arriving from the other direction.
pub(crate) fn npp_container_index(side: DockSide) -> u32 {
    match side {
        DockSide::Left => 0,
        DockSide::Right => 1,
        DockSide::Top => 2,
        DockSide::Bottom => 3,
    }
}

/// Upstream's count of docked containers, and so the first number a
/// floating container can have.
const DOCKCONT_MAX: u32 = 4;

/// The `nmhdr.code` for a panel now in `container`:
/// `MAKELONG(DMN_DOCK or DMN_FLOAT, container number)`.
///
/// The container number rides in the high word because that is where
/// upstream puts it, and where a plugin built from Notepad++'s
/// docking-dialog template reads it — `HIWORD(code)` on `DMN_DOCK` is
/// how that template learns which side it is docked to, and it
/// switches on `LOWORD(code)`, which is why the two halves must not be
/// swapped or merged. Floating containers are numbered from
/// [`DOCKCONT_MAX`] in the order the model lists floating groups; a
/// hidden panel whose remembered spot is floating is reported as the
/// container a new floating group would get. That number carries
/// less than the docked one — nothing in the template reads it — and
/// is reported because the code has to carry *something* there.
pub(crate) fn container_code(layout: &DockLayout, container: DockContainer) -> u32 {
    let (dmn, index) = match container {
        DockContainer::Docked(side) => (codepp_plugin_host::DMN_DOCK, npp_container_index(side)),
        DockContainer::Floating(group) => {
            let ordinal = group
                .and_then(|id| layout.floating_ordinal(id))
                .unwrap_or_else(|| layout.floating_groups().count());
            let ordinal = u32::try_from(ordinal).unwrap_or(u32::MAX);
            (
                codepp_plugin_host::DMN_FLOAT,
                DOCKCONT_MAX.saturating_add(ordinal).min(0xFFFF),
            )
        }
    };
    (index << 16) | (dmn & 0xFFFF)
}

/// Record, for every registered plugin panel, the container it is in
/// now, and return a `(h_client, code)` notification for each one
/// whose container differs from what its plugin was last told —
/// including every panel whose plugin has not been told anything yet,
/// which is how a freshly registered panel gets upstream's
/// registration-time notification.
///
/// Recording happens here, under the borrow, before
/// [`deliver_container_notices`] sends anything, and that order is
/// what stops a transition being told twice: a plugin's handler may
/// show or hide a panel, which reconciles again from inside the
/// delivery, and the nested pass must find the transition already
/// recorded. No `NPPM_*` a plugin can send moves an *existing* panel
/// between containers — show and hide keep it where it was — so for
/// those the nested pass has nothing to say. A source scan pins the
/// order.
///
/// Recording first does **not** bound the round trip on its own, and
/// an earlier version of this comment claimed it did. A handler that
/// *registers a new panel* gives the nested pass something genuinely
/// new — an entry nothing has been told about — so it produces a
/// notice of its own, whose handler may register another. That chain
/// is bounded only by the 64-panel registration cap, and each link is
/// a full trip through the main window procedure plus whatever stack
/// the plugin chooses to spend, which is a stack overflow — a hardware
/// exception nothing here catches — rather than a bound. The queue in
/// `deliver_container_notices` is what closes it.
pub(crate) fn container_notices(
    layout: &DockLayout,
    dialogs: &mut [crate::DockEntry],
) -> Vec<(HWND, u32)> {
    let mut out = Vec::new();
    for entry in dialogs.iter_mut() {
        let now = layout.container_of(entry.panel);
        let told = entry.dmn_container.is_some_and(|last| last.is_same(now));
        entry.dmn_container = Some(now);
        if !told {
            out.push((entry.h_client, container_code(layout, now)));
        }
    }
    out
}

/// Send each `DMN_DOCK` / `DMN_FLOAT` notice, as upstream does: a
/// plain `WM_NOTIFY` to the plugin's own `h_client`, `wParam` 0, with
/// `nmhdr.hwndFrom` the **main window** and `nmhdr.idFrom` 0 —
/// measured field by field against Notepad++ 8.9.6 with a probe
/// plugin loaded into both hosts.
///
/// `hwndFrom` is the load-bearing field: Notepad++'s docking-dialog
/// template, which most plugins with a panel are built on, ignores any
/// `WM_NOTIFY` whose `hwndFrom` is not the main window it was
/// initialised with. (Notepad++ sends a different family —
/// `DMN_SWITCHIN` and friends — from the container; those are not
/// sent here.)
///
/// **Notices raised while one is being delivered are queued, not
/// sent.** The plugin's handler runs inside the send with no borrow
/// held, so it may send `NPPM_*` back — including
/// `NPPM_DMMREGASDCKDLG` for a panel nothing has been told about, which
/// reconciles again from inside this loop and raises a notice of its
/// own. Sent there and then, that notice's handler could do the same,
/// nesting a full window-procedure round trip per link until the
/// registration cap or the stack ran out, whichever came first. So a
/// call made while a delivery is already running on this thread only
/// appends to the queue and returns, and the outermost call drains it
/// in order. The window-procedure nesting this path can cause is one
/// level, whatever the plugin does.
///
/// Unlike `DmnCloseGuard`, which *skips* a nested `DMN_CLOSE`, nothing
/// is dropped here: the record is already written by the time a notice
/// is queued, so a skipped notice would never be sent at all, and a
/// plugin registering from inside a handler would never learn where
/// its panel is. The cost is timing, and only in that nested case: the
/// panel registered inside a handler is told its container after that
/// handler returns, rather than before its `NPPM_DMMREGASDCKDLG`
/// returns. FIFO order also means that if a nested reconcile records a
/// newer container for a panel whose older notice is still waiting,
/// the plugin receives both in record order and ends on the latest.
///
/// # Safety
///
/// `main_hwnd` must be the main window HWND. UI thread only, with no
/// `WindowState` borrow held — the plugin's handler runs inside the
/// send and may call straight back into the host.
unsafe fn deliver_container_notices(main_hwnd: HWND, notices: Vec<(HWND, u32)>) {
    CONTAINER_NOTICES.with(|q| {
        q.borrow_mut()
            .extend(notices.into_iter().map(|(h, code)| (main_hwnd, h, code)));
    });
    // A delivery is already running further up this thread's stack:
    // it will send what was just queued once its current handler
    // returns.
    let Some(_delivering) = NoticeDelivery::enter() else {
        return;
    };
    // Popped one at a time, with the queue's borrow released before
    // the send — the handler may queue more, which needs the borrow.
    while let Some((main_hwnd, h_client, code)) =
        CONTAINER_NOTICES.with(|q| q.borrow_mut().pop_front())
    {
        // A handler for an earlier notice may have destroyed this
        // one's window; the record is already written, so skipping
        // it loses nothing that could still be delivered.
        if !unsafe { IsWindow(Some(h_client)) }.as_bool() {
            continue;
        }
        // The one record of what a plugin was told about its panel's
        // position; a plugin reports its reaction only through its own
        // state, so this is what connects the two when one misbehaves.
        tracing::debug!(
            h_client = h_client.0 as usize,
            dmn = if code & 0xFFFF == codepp_plugin_host::DMN_DOCK {
                "DMN_DOCK"
            } else {
                "DMN_FLOAT"
            },
            container = code >> 16,
            "dock container notification"
        );
        let nmhdr = NMHDR {
            hwndFrom: main_hwnd,
            idFrom: 0,
            code,
        };
        unsafe {
            SendMessageW(
                h_client,
                WM_NOTIFY,
                Some(WPARAM(0)),
                Some(LPARAM(&raw const nmhdr as isize)),
            );
        }
    }
}

thread_local! {
    /// `(main window, h_client, code)` notices waiting to be sent. See
    /// [`deliver_container_notices`].
    static CONTAINER_NOTICES: std::cell::RefCell<std::collections::VecDeque<(HWND, HWND, u32)>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    /// Set while [`deliver_container_notices`] is draining on this
    /// thread. See [`NoticeDelivery`].
    static CONTAINER_NOTICES_DELIVERING: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Marks a `DMN_DOCK` / `DMN_FLOAT` drain as running on this thread,
/// so a nested call queues instead of sending. RAII for the same
/// reason as `DmnCloseGuard` and `DrainFreeze`: the flag must clear on
/// the unwinding path too, or one caught panic leaves every later
/// notice queued behind a drain that no longer exists.
///
/// **On that path it also discards whatever is still queued**, which
/// is where this differs from `DrainFreeze`: that guard defers channel
/// work that is safe to replay whenever, while these entries are raw
/// window handles, and Windows reuses handle values. A notice left
/// behind could be delivered later to an unrelated window that has
/// since been given the same handle, which `IsWindow` cannot tell
/// apart. Dropping it costs that plugin one notice after something has
/// already gone wrong. How an unwind could reach here at all is narrow.
/// Nothing reached *through* the send can produce one: every window
/// procedure — the plugin's, and the host's own when a handler sends
/// `NPPM_*` back — is a plain `extern "system"` function, and a panic
/// inside one aborts at that function's own boundary rather than
/// unwinding back out of `SendMessageW`. (That is also why no
/// `catch_unwind` sits around the send: it could never catch anything.
/// It is not because the host's procedures guard themselves; most do
/// not.) So an unwind would have to start in this loop itself, and
/// release builds abort on panic regardless.
struct NoticeDelivery;

impl NoticeDelivery {
    /// `None` when a drain is already running on this thread.
    fn enter() -> Option<Self> {
        if CONTAINER_NOTICES_DELIVERING.with(std::cell::Cell::get) {
            return None;
        }
        CONTAINER_NOTICES_DELIVERING.with(|f| f.set(true));
        Some(Self)
    }
}

impl Drop for NoticeDelivery {
    fn drop(&mut self) {
        CONTAINER_NOTICES_DELIVERING.with(|f| f.set(false));
        // The normal path leaves the queue empty — the drain only ends
        // when `pop_front` does — so this only ever discards on an
        // unwind. `try_` throughout because a destructor must not panic
        // and a plain `borrow_mut` would if an unwind ever began with a
        // borrow of the queue live. None does today — every borrow in
        // `deliver_container_notices` ends before the next statement —
        // but that is a property of the loop's current shape, not of
        // anything this destructor can check.
        if std::thread::panicking() {
            let _ = CONTAINER_NOTICES.try_with(|q| {
                if let Ok(mut q) = q.try_borrow_mut() {
                    q.clear();
                }
            });
        }
    }
}

pub(crate) fn side_index(side: DockSide) -> usize {
    match side {
        DockSide::Left => 0,
        DockSide::Right => 1,
        DockSide::Top => 2,
        DockSide::Bottom => 3,
    }
}

/// Push the two panels' visibility into the toolbar toggle buttons
/// (the View-menu marks resolve live at open time).
pub(crate) unsafe fn sync_dock_indicators(main_hwnd: HWND) {
    unsafe {
        let Some((toolbar_hwnd, ws, dm)) = state_from_hwnd(main_hwnd).map(|state| {
            (
                state.toolbar_hwnd,
                state.dock_layout.is_visible(DockPanel::Workspace),
                state.dock_layout.is_visible(DockPanel::DocMap),
            )
        }) else {
            return;
        };
        toolbar::set_button_checked(toolbar_hwnd, ID_VIEW_FOLDER_AS_WORKSPACE, ws);
        toolbar::set_button_checked(toolbar_hwnd, ID_VIEW_DOCMAP, dm);
    }
}

// --- drag machinery -------------------------------------------------------------

/// The size a panel or group opens at when it is torn off into a
/// float: a third of the main window's current width and height,
/// floored at the model's minimum — the GTK choice, for the reason it
/// records (a full-height side band makes an awkward window nobody
/// wants and has to resize anyway).
#[must_use]
fn tear_off_size(main: (i32, i32)) -> (i32, i32) {
    ((main.0 / 3).max(MIN_FLOAT_W), (main.1 / 3).max(MIN_FLOAT_H))
}

/// Where the pointer sits inside a torn-off float's caption: the same
/// *fraction* along the caption as it had along the source (so a grab
/// near the right end stays near the right end), clamped inside the
/// new width, and vertically mid-caption.
#[must_use]
fn tear_off_grab(cursor_dx: i32, source_w: i32, new_w: i32) -> (i32, i32) {
    let fraction = f64::from(cursor_dx.clamp(0, source_w.max(1))) / f64::from(source_w.max(1));
    let x = (fraction * f64::from(new_w)).round() as i32;
    (x.clamp(0, new_w.max(0)), DOCK_CAPTION_H / 2)
}

/// The dock area (between toolbar-bottom and status-bar-top) in
/// screen coordinates — the coordinate space every drag computation
/// uses, because floating groups and the hint are screen-positioned
/// popups.
unsafe fn dock_area_screen(main_hwnd: HWND, toolbar_height: i32) -> DockRect {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(main_hwnd, &raw mut rc);
        let mut origin = POINT { x: 0, y: 0 };
        let _ = ClientToScreen(main_hwnd, &raw mut origin);
        DockRect::new(
            origin.x,
            origin.y + toolbar_height,
            rc.right.max(0),
            (rc.bottom - toolbar_height - STATUS_HEIGHT_PX).max(0),
        )
    }
}

fn rect_of(hwnd: HWND) -> DockRect {
    let mut rc = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &raw mut rc) };
    DockRect::new(rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top)
}

/// Resolve the drop target + hint rect for the current cursor
/// position. Builds the `DropZones` from live window rects
/// (floating groups first — they sit above the docked ones) and
/// delegates the decision to the pure core resolver.
unsafe fn resolve_current_drop(
    main_hwnd: HWND,
    cursor: (i32, i32),
) -> Option<(DropTarget, DockRect)> {
    unsafe {
        let (layout, groups, toolbar_height, drag_info) =
            state_from_hwnd(main_hwnd).map(|state| {
                (
                    state.dock_layout.clone(),
                    state.dock_groups.clone(),
                    toolbar::toolbar_height_px(state.toolbar_bitmap_px),
                    state
                        .dock_drag
                        .as_ref()
                        .map(|d| (d.subject, d.grab, d.float_size)),
                )
            })?;
        let (subject, grab, float_size) = drag_info?;
        let mid = dock_area_screen(main_hwnd, toolbar_height);
        let mut zone_groups: Vec<(u32, DockRect)> = Vec::new();
        for gw in groups.iter().filter(|gw| gw.floating) {
            zone_groups.push((gw.id, rect_of(gw.hwnd)));
        }
        for gw in groups.iter().filter(|gw| !gw.floating) {
            zone_groups.push((gw.id, rect_of(gw.hwnd)));
        }
        let zones = DropZones {
            mid,
            groups: zone_groups,
        };
        let preview = DockRect::new(
            cursor.0 - grab.0,
            cursor.1 - grab.1,
            float_size.0,
            float_size.1,
        );
        Some(resolve_drop(&zones, &layout, subject, cursor, preview))
    }
}

unsafe fn show_hint(hint: HWND, rect: DockRect) {
    unsafe {
        let _ = SetWindowPos(
            hint,
            Some(HWND_TOPMOST),
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

unsafe fn hide_hint(hint: HWND) {
    unsafe {
        let _ = ShowWindow(hint, SW_HIDE);
    }
}

// --- group window proc -----------------------------------------------------------

/// Measured label widths for a group's tabs (screen text metrics —
/// mechanism, so it lives here; the extents math it feeds is pure).
unsafe fn measure_tab_labels(ghwnd: HWND, panels: &[DockPanel]) -> Vec<i32> {
    unsafe {
        let hdc = GetDC(Some(ghwnd));
        let font = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let mut out = Vec::with_capacity(panels.len());
        for panel in panels {
            let wide: Vec<u16> = panel.title().encode_utf16().collect();
            let mut size = windows::Win32::Foundation::SIZE::default();
            let _ = GetTextExtentPoint32W(hdc, &wide, &raw mut size);
            out.push(size.cx);
        }
        SelectObject(hdc, old);
        ReleaseDC(Some(ghwnd), hdc);
        out
    }
}

/// Snapshot of what a group window needs to paint / hit-test,
/// taken under a brief state borrow and used after it drops.
struct GroupSnapshot {
    panels: Vec<DockPanel>,
    active: usize,
    floating: bool,
}

unsafe fn group_snapshot(ghwnd: HWND) -> Option<(HWND, GroupSnapshot)> {
    unsafe {
        let main = find_main_hwnd(ghwnd)?;
        let id = GetWindowLongPtrW(ghwnd, GWLP_USERDATA) as u32;
        let state = state_from_hwnd(main)?;
        let group = state.dock_layout.group(id)?;
        let floating = matches!(group.location, DockLocation::Floating(_));
        Some((
            main,
            GroupSnapshot {
                panels: group.panels.clone(),
                active: group.active,
                floating,
            },
        ))
    }
}

extern "system" fn dock_group_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_SIZE => {
                on_group_size(hwnd);
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                // Floating groups are user-resizable via the thick
                // frame; keep the caption reachable and the content
                // usable at the same floors the model enforces.
                let mmi = lparam.0 as *mut MINMAXINFO;
                if !mmi.is_null() {
                    (*mmi).ptMinTrackSize.x = MIN_FLOAT_W;
                    (*mmi).ptMinTrackSize.y = MIN_FLOAT_H;
                }
                LRESULT(0)
            }
            WM_PAINT => {
                paint_group(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                on_group_button_down(hwnd, lparam);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                on_group_mouse_move(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                on_group_button_up(hwnd);
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                // Capture stolen mid-gesture (Alt+Tab, a modal) —
                // abandon it without committing anything.
                if let Some(main) = find_main_hwnd(hwnd) {
                    let hint = state_from_hwnd(main).map(|state| {
                        state.dock_drag = None;
                        state.dock_hint_hwnd
                    });
                    if let Some(hint) = hint {
                        hide_hint(hint);
                    }
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Position the active panel's content inside the group, mirror a
/// floating resize back into the model, and repaint the chrome.
unsafe fn on_group_size(ghwnd: HWND) {
    unsafe {
        let Some((main, snap)) = group_snapshot(ghwnd) else {
            return;
        };
        // Snapshot the content HWND under the same borrow, then
        // drop it before MoveWindow (which cascades WM_SIZE into
        // the panel procs — the standard aliasing discipline).
        let (content, float_rect) = {
            let Some(state) = state_from_hwnd(main) else {
                return;
            };
            let content = snap
                .panels
                .get(snap.active)
                .map(|p| panel_content_hwnd(state, *p));
            (content, snap.floating.then(|| rect_of(ghwnd)))
        };
        let mut rc = RECT::default();
        let _ = GetClientRect(ghwnd, &raw mut rc);
        let content_rc = group_content_rect(rc.right, rc.bottom, snap.panels.len());
        if let Some(content) = content {
            if !content.0.is_null() {
                let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(
                    content,
                    content_rc.x,
                    content_rc.y,
                    content_rc.w,
                    content_rc.h,
                    true,
                );
            }
        }
        // A floating group's frame is the user's resize handle;
        // keep the model's rect current so persistence and the
        // next reconcile agree with what is on screen. (Our own
        // SetWindowPos lands here too — same rect, no-op write.)
        if let Some(rect) = float_rect {
            let id = GetWindowLongPtrW(ghwnd, GWLP_USERDATA) as u32;
            if let Some(state) = state_from_hwnd(main) {
                state.dock_layout.set_floating_rect(id, rect);
            }
        }
        let _ = InvalidateRect(Some(ghwnd), None, true);
    }
}

unsafe fn paint_group(ghwnd: HWND) {
    unsafe {
        let snapshot = group_snapshot(ghwnd);
        // Icon bitmaps live on the main state; grab them, and the
        // per-plugin ones, in the same brief borrow.
        let (icons, plugin_icons) = snapshot
            .as_ref()
            .and_then(|(main, _)| state_from_hwnd(*main))
            .map_or((None, Vec::new()), |state| {
                (
                    Some(state.dock_tab_icons),
                    // Empty for the overwhelmingly common case of a
                    // group holding only built-in panels, so the
                    // allocation is skipped on most repaints.
                    if state.dock_dialogs.is_empty() {
                        Vec::new()
                    } else {
                        state
                            .dock_dialogs
                            .iter()
                            .filter_map(|e| e.tab_icon.map(|bmp| (e.panel, bmp)))
                            .collect()
                    },
                )
            });
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(ghwnd, &raw mut ps);
        if let Some((_, snap)) = snapshot {
            let mut rc = RECT::default();
            let _ = GetClientRect(ghwnd, &raw mut rc);
            draw_group_chrome(hdc, ghwnd, rc.right, rc.bottom, &snap, icons, &plugin_icons);
        }
        let _ = EndPaint(ghwnd, &raw const ps);
    }
}

unsafe fn draw_group_chrome(
    hdc: HDC,
    ghwnd: HWND,
    w: i32,
    h: i32,
    snap: &GroupSnapshot,
    icons: Option<[HBITMAP; 3]>,
    plugin_icons: &[(DockPanel, HBITMAP)],
) {
    unsafe {
        let font = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
        let old_font = SelectObject(hdc, HGDIOBJ(font.0));
        SetBkMode(hdc, TRANSPARENT);

        // Caption band.
        let cap = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: DOCK_CAPTION_H,
        };
        FillRect(hdc, &raw const cap, GetSysColorBrush(COLOR_3DFACE));
        let title = snap.panels.get(snap.active).map_or("", |p| p.title());
        let mut wide: Vec<u16> = title.encode_utf16().collect();
        let mut title_rc = RECT {
            left: 6,
            top: 0,
            right: (w - DOCK_CLOSE_W - 4).max(6),
            bottom: DOCK_CAPTION_H,
        };
        DrawTextW(
            hdc,
            &mut wide,
            &raw mut title_rc,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        let close = caption_close_rect(w);
        let mut close_rc = RECT {
            left: close.x,
            top: close.y,
            right: close.x + close.w,
            bottom: close.y + close.h,
        };
        let mut glyph: Vec<u16> = "\u{2715}".encode_utf16().collect();
        DrawTextW(
            hdc,
            &mut glyph,
            &raw mut close_rc,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
        );

        // Tab bar (2+ panels only).
        if snap.panels.len() > 1 {
            let bar_top = h - DOCK_TAB_BAR_H;
            let bar = RECT {
                left: 0,
                top: bar_top,
                right: w,
                bottom: h,
            };
            FillRect(hdc, &raw const bar, GetSysColorBrush(COLOR_3DFACE));
            let labels = measure_tab_labels(ghwnd, &snap.panels);
            let extents = tab_extents(&labels, snap.active);
            for (i, (panel, (tx, tw))) in snap.panels.iter().zip(&extents).enumerate() {
                let tab_rc = RECT {
                    left: *tx,
                    top: bar_top,
                    right: tx + tw,
                    bottom: h,
                };
                if i == snap.active {
                    FillRect(hdc, &raw const tab_rc, GetSysColorBrush(COLOR_WINDOW));
                }
                let icon_y = bar_top + (DOCK_TAB_BAR_H - DOCK_TAB_ICON_PX) / 2;
                // A plugin's own icon wins over the built-in glyph.
                // It is what tells two plugin panels apart at all: an
                // inactive tab is icon-only, so without this every
                // plugin tab in a group looks identical (and looks
                // like the Document Map, which is the placeholder
                // they all shared).
                let bitmap = plugin_icons
                    .iter()
                    .find(|(p, _)| p == panel)
                    .map(|(_, bmp)| *bmp)
                    .or_else(|| icons.map(|i| i[panel_icon_index(*panel)]));
                if let Some(bitmap) = bitmap {
                    blit_icon(hdc, bitmap, tx + DOCK_TAB_PAD, icon_y);
                }
                if i == snap.active {
                    let mut label: Vec<u16> = panel.title().encode_utf16().collect();
                    let mut label_rc = RECT {
                        left: tx + DOCK_TAB_PAD + DOCK_TAB_ICON_PX + DOCK_TAB_ICON_GAP,
                        top: bar_top,
                        right: tx + tw - DOCK_TAB_PAD,
                        bottom: h,
                    };
                    DrawTextW(
                        hdc,
                        &mut label,
                        &raw mut label_rc,
                        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                    );
                }
            }
        }
        SelectObject(hdc, old_font);
    }
}

/// `AlphaBlend` a premultiplied-BGRA icon bitmap (any source size)
/// into a `DOCK_TAB_ICON_PX` square — the same scale-on-blit the
/// editor tab strip uses for its save glyph.
unsafe fn blit_icon(hdc: HDC, bitmap: HBITMAP, x: i32, y: i32) {
    unsafe {
        if bitmap.is_invalid() {
            return;
        }
        let mut bm = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&raw mut bm).cast::<c_void>()),
        ) == 0
        {
            return;
        }
        let mem = CreateCompatibleDC(Some(hdc));
        let old = SelectObject(mem, HGDIOBJ(bitmap.0));
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = AlphaBlend(
            hdc,
            x,
            y,
            DOCK_TAB_ICON_PX,
            DOCK_TAB_ICON_PX,
            mem,
            0,
            0,
            bm.bmWidth,
            bm.bmHeight,
            blend,
        );
        SelectObject(mem, old);
        let _ = DeleteDC(mem);
    }
}

unsafe fn on_group_button_down(ghwnd: HWND, lparam: LPARAM) {
    unsafe {
        let Some((main, snap)) = group_snapshot(ghwnd) else {
            return;
        };
        let x = i32::from((lparam.0 & 0xFFFF) as i16);
        let y = i32::from(((lparam.0 >> 16) & 0xFFFF) as i16);
        let mut rc = RECT::default();
        let _ = GetClientRect(ghwnd, &raw mut rc);

        // A click anywhere on a floating group's chrome raises it
        // above its floating siblings (owned popups share a band
        // above the owner; z within the band is click-to-front).
        if snap.floating {
            let _ = SetWindowPos(
                ghwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }

        let id = GetWindowLongPtrW(ghwnd, GWLP_USERDATA) as u32;
        let outer = rect_of(ghwnd);
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&raw mut cursor);

        let (subject, armed_tab, armed_close, grab, float_size) = if y < DOCK_CAPTION_H {
            let close = caption_close_rect(rc.right);
            let armed_close = close.contains(x, y);
            // Caption drag = whole group. An already-floating group
            // is being *moved*: it keeps its size, and the grab
            // offset keeps the cursor where the user pressed,
            // relative to the window's own origin, so the window
            // moves "in hand". A docked group is being torn off: it
            // opens at [`tear_off_size`] with the pointer at the
            // same fraction along the caption ([`tear_off_grab`]).
            let (grab, float_size) = if snap.floating {
                (
                    (cursor.x - outer.x, cursor.y - outer.y),
                    (outer.w.max(MIN_FLOAT_W), outer.h.max(MIN_FLOAT_H)),
                )
            } else {
                let main_rect = rect_of(main);
                let size = tear_off_size((main_rect.w, main_rect.h));
                (tear_off_grab(cursor.x - outer.x, outer.w, size.0), size)
            };
            (DragSubject::Group(id), None, armed_close, grab, float_size)
        } else if snap.panels.len() > 1 && y >= rc.bottom - DOCK_TAB_BAR_H {
            let labels = measure_tab_labels(ghwnd, &snap.panels);
            let extents = tab_extents(&labels, snap.active);
            let Some(tab) = hit_tab(&extents, x) else {
                return;
            };
            let Some(panel) = snap.panels.get(tab).copied() else {
                return;
            };
            let main_rect = rect_of(main);
            let size = tear_off_size((main_rect.w, main_rect.h));
            (
                DragSubject::Panel(panel),
                Some(tab),
                false,
                // A torn-off tab floats at the tear-off size with
                // the grab point mid-caption.
                (size.0 / 2, DOCK_CAPTION_H / 2),
                size,
            )
        } else {
            return;
        };

        if let Some(state) = state_from_hwnd(main) {
            state.dock_drag = Some(DockDrag {
                subject,
                group_id: id,
                start: (cursor.x, cursor.y),
                grab,
                float_size,
                armed_tab,
                armed_close,
                started: false,
                cancelled: false,
            });
        }
        let _ = SetCapture(ghwnd);
    }
}

unsafe fn on_group_mouse_move(ghwnd: HWND) {
    unsafe {
        let Some(main) = find_main_hwnd(ghwnd) else {
            return;
        };
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&raw mut cursor);

        // Phase 1 (borrow): advance the gesture state machine.
        let Some((hint, act)) = state_from_hwnd(main).and_then(|state| {
            let hint = state.dock_hint_hwnd;
            let drag = state.dock_drag.as_mut()?;
            if drag.cancelled {
                return None;
            }
            // Esc cancels a live drag without committing.
            if GetKeyState(
                windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE
                    .0
                    .into(),
            ) < 0
            {
                drag.cancelled = true;
                return Some((hint, MoveAction::CancelHint));
            }
            if !drag.started {
                let dx = (cursor.x - drag.start.0).abs();
                let dy = (cursor.y - drag.start.1).abs();
                if dx < DOCK_DRAG_THRESHOLD && dy < DOCK_DRAG_THRESHOLD {
                    return None;
                }
                drag.started = true;
                // Once it is a drag, it is no longer a click.
                drag.armed_tab = None;
                drag.armed_close = false;
            }
            Some((hint, MoveAction::UpdateHint))
        }) else {
            return;
        };

        // Phase 2 (no borrow): drive the hint window.
        match act {
            MoveAction::CancelHint => hide_hint(hint),
            MoveAction::UpdateHint => {
                if let Some((_, rect)) = resolve_current_drop(main, (cursor.x, cursor.y)) {
                    show_hint(hint, rect);
                }
            }
        }
    }
}

enum MoveAction {
    UpdateHint,
    CancelHint,
}

unsafe fn on_group_button_up(ghwnd: HWND) {
    unsafe {
        let Some(main) = find_main_hwnd(ghwnd) else {
            let _ = ReleaseCapture();
            return;
        };
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&raw mut cursor);

        // Take the gesture out of state BEFORE releasing capture.
        // `ReleaseCapture` synchronously delivers `WM_CAPTURECHANGED`
        // to this window, whose handler sets `dock_drag = None` — so
        // releasing first would clear the gesture we are about to
        // read and every drop would silently no-op. Taking it first
        // makes that handler's clear a no-op instead.
        let taken = state_from_hwnd(main).and_then(|state| {
            let hint = state.dock_hint_hwnd;
            state.dock_drag.take().map(|d| (d, hint))
        });
        let _ = ReleaseCapture();
        let Some((drag, hint)) = taken else {
            return;
        };
        hide_hint(hint);
        if drag.cancelled {
            return;
        }

        if drag.started {
            // Recompute at the release point — the hint followed
            // the cursor, but the model mutation must key on where
            // the button actually went up. The drag record is gone
            // from state, so rebuild the preview locally.
            let preview = DockRect::new(
                cursor.x - drag.grab.0,
                cursor.y - drag.grab.1,
                drag.float_size.0,
                drag.float_size.1,
            );
            let target = {
                let Some(state) = state_from_hwnd(main) else {
                    return;
                };
                let layout = state.dock_layout.clone();
                let groups = state.dock_groups.clone();
                let toolbar_height = toolbar::toolbar_height_px(state.toolbar_bitmap_px);
                drop_target_for(
                    main,
                    &layout,
                    &groups,
                    toolbar_height,
                    drag.subject,
                    (cursor.x, cursor.y),
                    preview,
                )
            };
            if let Some(state) = state_from_hwnd(main) {
                match drag.subject {
                    DragSubject::Panel(panel) => state.dock_layout.move_panel(panel, target),
                    DragSubject::Group(id) => state.dock_layout.move_group(id, target),
                }
            }
            apply_dock_layout(main);
            return;
        }

        // Click gestures: close and tab switch resolve only if the
        // release is still on what was pressed.
        let mut pt = cursor;
        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(ghwnd, &raw mut pt);
        let mut rc = RECT::default();
        let _ = GetClientRect(ghwnd, &raw mut rc);
        if drag.armed_close && caption_close_rect(rc.right).contains(pt.x, pt.y) {
            let active = state_from_hwnd(main).and_then(|state| {
                state
                    .dock_layout
                    .group(drag.group_id)
                    .map(codepp_core::dock::DockGroup::active_panel)
            });
            if let Some(panel) = active {
                crate::dock_close_panel(main, panel);
            }
            return;
        }
        if let Some(tab) = drag.armed_tab {
            let still_on_it = {
                let snap = group_snapshot(ghwnd).map(|(_, s)| s);
                snap.is_some_and(|snap| {
                    let labels = measure_tab_labels(ghwnd, &snap.panels);
                    let extents = tab_extents(&labels, snap.active);
                    pt.y >= rc.bottom - DOCK_TAB_BAR_H && hit_tab(&extents, pt.x) == Some(tab)
                })
            };
            if still_on_it {
                if let Some(state) = state_from_hwnd(main) {
                    state.dock_layout.set_active_index(drag.group_id, tab);
                }
                apply_dock_layout(main);
            }
        }
    }
}

/// Free-function variant of [`resolve_current_drop`] for the
/// button-up path, where the drag record has already been taken
/// out of state.
unsafe fn drop_target_for(
    main_hwnd: HWND,
    layout: &DockLayout,
    groups: &[DockGroupWindow],
    toolbar_height: i32,
    subject: DragSubject,
    cursor: (i32, i32),
    preview: DockRect,
) -> DropTarget {
    unsafe {
        let mid = dock_area_screen(main_hwnd, toolbar_height);
        let mut zone_groups: Vec<(u32, DockRect)> = Vec::new();
        for gw in groups.iter().filter(|gw| gw.floating) {
            zone_groups.push((gw.id, rect_of(gw.hwnd)));
        }
        for gw in groups.iter().filter(|gw| !gw.floating) {
            zone_groups.push((gw.id, rect_of(gw.hwnd)));
        }
        let zones = DropZones {
            mid,
            groups: zone_groups,
        };
        resolve_drop(&zones, layout, subject, cursor, preview).0
    }
}

// --- hint window proc ---------------------------------------------------------

extern "system" fn dock_hint_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                // The class brush fills the grey body (made
                // translucent by the layered alpha); paint the
                // darker outline on top so the preview reads as a
                // bounded box rather than a smudge.
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &raw mut ps);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &raw mut rc);
                let frame = GetSysColorBrush(windows::Win32::Graphics::Gdi::COLOR_BTNSHADOW);
                for _ in 0..1 {
                    // 2-px frame drawn as four FillRect strips.
                    let t = 2;
                    let top = RECT {
                        left: 0,
                        top: 0,
                        right: rc.right,
                        bottom: t,
                    };
                    let bottom = RECT {
                        left: 0,
                        top: rc.bottom - t,
                        right: rc.right,
                        bottom: rc.bottom,
                    };
                    let left = RECT {
                        left: 0,
                        top: 0,
                        right: t,
                        bottom: rc.bottom,
                    };
                    let right = RECT {
                        left: rc.right - t,
                        top: 0,
                        right: rc.right,
                        bottom: rc.bottom,
                    };
                    FillRect(hdc, &raw const top, frame);
                    FillRect(hdc, &raw const bottom, frame);
                    FillRect(hdc, &raw const left, frame);
                    FillRect(hdc, &raw const right, frame);
                }
                let _ = EndPaint(hwnd, &raw const ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// --- side splitter proc ---------------------------------------------------------

fn side_from_index(i: usize) -> DockSide {
    match i {
        0 => DockSide::Left,
        1 => DockSide::Right,
        2 => DockSide::Top,
        _ => DockSide::Bottom,
    }
}

extern "system" fn dock_side_splitter_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let side = side_from_index(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as usize);
        match msg {
            WM_SETCURSOR => {
                let cursor_id = match side {
                    DockSide::Left | DockSide::Right => IDC_SIZEWE,
                    DockSide::Top | DockSide::Bottom => IDC_SIZENS,
                };
                if let Ok(cursor) = LoadCursorW(None, cursor_id) {
                    let _ = windows::Win32::UI::WindowsAndMessaging::SetCursor(Some(cursor));
                }
                LRESULT(1)
            }
            WM_LBUTTONDOWN => {
                if let Some(main) = find_main_hwnd(hwnd) {
                    let mut pt = POINT::default();
                    if GetCursorPos(&raw mut pt).is_ok() {
                        if let Some(state) = state_from_hwnd(main) {
                            state.dock_side_drag = Some(DockSideDrag {
                                side,
                                start: (pt.x, pt.y),
                                size_at_start: state.dock_layout.side_size(side),
                            });
                            let _ = SetCapture(hwnd);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let Some(main) = find_main_hwnd(hwnd) else {
                    return LRESULT(0);
                };
                let mut pt = POINT::default();
                if GetCursorPos(&raw mut pt).is_err() {
                    return LRESULT(0);
                }
                // Compute the visually-clamped result via the same
                // authority the layout uses, then store THAT — so
                // the model's size always matches what renders and
                // a drag past the limit has no dead zone on the
                // way back.
                let apply = state_from_hwnd(main).and_then(|state| {
                    let drag = state.dock_side_drag?;
                    let proposed = side_drag_size(
                        drag.side,
                        drag.size_at_start,
                        (pt.x - drag.start.0, pt.y - drag.start.1),
                    );
                    let toolbar_height = toolbar::toolbar_height_px(state.toolbar_bitmap_px);
                    let mut rc = RECT::default();
                    let _ = GetClientRect(main, &raw mut rc);
                    let mid = DockRect::new(
                        0,
                        toolbar_height,
                        rc.right,
                        (rc.bottom - toolbar_height - STATUS_HEIGHT_PX).max(0),
                    );
                    let mut probe = state.dock_layout.clone();
                    probe.set_side_size(drag.side, proposed);
                    let frame = compute_frame(
                        mid,
                        &probe,
                        crate::MIN_SCINTILLA_WIDTH_PX,
                        crate::MIN_SCINTILLA_HEIGHT_PX,
                    );
                    let rendered = frame.bands.iter().find(|b| b.side == drag.side).map(|b| {
                        match drag.side {
                            DockSide::Left | DockSide::Right => b.rect.w,
                            DockSide::Top | DockSide::Bottom => b.rect.h,
                        }
                    })?;
                    if rendered == state.dock_layout.side_size(drag.side) {
                        return None;
                    }
                    state.dock_layout.set_side_size(drag.side, rendered);
                    Some(state.scintilla_hwnd)
                });
                if let Some(scintilla) = apply {
                    {
                        let _redraw = crate::ScintillaRedrawGuard::enter(scintilla);
                        relayout_now(main);
                    }
                    let _ = windows::Win32::Graphics::Gdi::UpdateWindow(scintilla);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP | WM_CAPTURECHANGED => {
                if let Some(main) = find_main_hwnd(hwnd) {
                    if let Some(state) = state_from_hwnd(main) {
                        state.dock_side_drag = None;
                        // Splitter release is a good moment to
                        // write the new size through to the
                        // session cache.
                        let session = state.dock_layout.to_session();
                        state.shell.set_dock_session(Some(session));
                    }
                }
                if msg == WM_LBUTTONUP {
                    let _ = ReleaseCapture();
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The container numbers are ABI in both directions: a plugin
    /// names a side through `DWS_DF_CONT_*` when it registers, and is
    /// told its side back through `DMN_DOCK`'s high word. The two
    /// must be one numbering, or a plugin asking for the bottom is
    /// told it is docked on the right.
    #[test]
    fn container_numbers_match_the_registration_nibble() {
        for side in DockSide::ALL {
            let n = npp_container_index(side);
            assert_eq!(
                crate::dock_side_from_u_mask(n << 28),
                Some(side),
                "container {n} decodes to a different side than it encodes"
            );
        }
    }

    #[test]
    fn container_code_packs_the_notification_low_and_the_container_high() {
        let layout = DockLayout::new();
        let docked = container_code(&layout, DockContainer::Docked(DockSide::Bottom));
        assert_eq!(docked & 0xFFFF, codepp_plugin_host::DMN_DOCK);
        assert_eq!(docked >> 16, 3, "CONT_BOTTOM");
        let left = container_code(&layout, DockContainer::Docked(DockSide::Left));
        assert_eq!(
            left,
            codepp_plugin_host::DMN_DOCK,
            "CONT_LEFT is 0: the bare code"
        );
        let floating = container_code(&layout, DockContainer::Floating(None));
        assert_eq!(floating & 0xFFFF, codepp_plugin_host::DMN_FLOAT);
        assert_eq!(floating >> 16, DOCKCONT_MAX, "first floating container");
    }

    #[test]
    fn container_code_numbers_floating_groups_after_the_docked_four() {
        let a = codepp_core::dock::intern_plugin_panel("cc-a.dll", "CC A").expect("intern");
        let b = codepp_core::dock::intern_plugin_panel("cc-b.dll", "CC B").expect("intern");
        let mut l = DockLayout::new();
        l.show(a);
        l.show(b);
        l.move_panel(a, DropTarget::Floating(DockRect::new(0, 0, 300, 200)));
        l.move_panel(b, DropTarget::Floating(DockRect::new(40, 40, 300, 200)));
        let code_of = |p| container_code(&l, l.container_of(p)) >> 16;
        assert_eq!(code_of(a), DOCKCONT_MAX);
        assert_eq!(code_of(b), DOCKCONT_MAX + 1);
    }

    /// What bounds the `DMN_DOCK` / `DMN_FLOAT` round trip is an
    /// ordering no unit test can see: the container is *recorded*
    /// under the borrow, in `container_notices`, and only then sent,
    /// with none held, by `deliver_container_notices`. A handler that
    /// shows or hides a panel reconciles again from inside the send;
    /// if the record were written after the send, that nested pass
    /// would find the transition untold and send it again, from inside
    /// which the next pass would do the same.
    #[test]
    fn the_container_is_recorded_before_the_notification_is_sent() {
        use crate::plugin_reentry_guards::{code_only, fn_body};
        let src = include_str!("dock_panels.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("test module")];

        let apply = code_only(&fn_body(src, "apply_dock_layout"));
        let record = apply
            .find("container_notices(&state.dock_layout")
            .expect("the reconcile no longer records containers under the borrow");
        let send = apply
            .find("deliver_container_notices(main_hwnd")
            .expect("the reconcile no longer sends the notifications");
        assert!(record < send, "the send now precedes the record");

        let notices = code_only(&fn_body(src, "container_notices"));
        assert!(
            notices.contains("entry.dmn_container = Some(now);"),
            "container_notices no longer writes the record"
        );
        assert!(
            !notices.contains("SendMessageW"),
            "container_notices sends while the caller's borrow is live"
        );
        let deliver = code_only(&fn_body(src, "deliver_container_notices"));
        assert!(
            !deliver.contains("dmn_container"),
            "the record moved into the send loop, after the send it must precede"
        );
    }

    /// A registration with nothing behind it but a panel identity and
    /// a handle value, for the policy tests below. The handle is never
    /// dereferenced by them.
    ///
    /// Built here rather than as a `#[cfg(test)]` impl beside
    /// `DockEntry` in `lib.rs`: that file's source-scan guards read
    /// everything above its first `#[cfg(test)]`, and a test-only
    /// block near the top truncates what every one of them sees.
    fn entry_for_test(panel: DockPanel, h_client: HWND) -> crate::DockEntry {
        crate::DockEntry {
            panel,
            tb_data: core::ptr::null(),
            h_client,
            name: String::new(),
            module_name: String::new(),
            dlg_id: 0,
            tab_icon: None,
            u_mask: 0,
            dmn_container: None,
        }
    }

    /// The bound the audit found missing, driven through real
    /// `SendMessageW` into a real window: every notice's handler raises
    /// a new one, which is what a plugin registering a fresh panel from
    /// its `DMN_DOCK` handler causes. Each must still arrive, in order,
    /// and none may be delivered from inside another's handler.
    #[test]
    fn a_notice_raised_during_delivery_is_queued_not_nested() {
        use std::cell::{Cell, RefCell};
        use windows::Win32::UI::WindowsAndMessaging::HWND_MESSAGE;

        const CHAIN: u32 = 12;
        thread_local! {
            static DEPTH: Cell<u32> = const { Cell::new(0) };
            static MAX_DEPTH: Cell<u32> = const { Cell::new(0) };
            static SEEN: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
        }
        unsafe extern "system" fn probe_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if msg != WM_NOTIFY {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            // SAFETY: `deliver_container_notices` passes a live NMHDR.
            let code = unsafe { (*(lparam.0 as *const NMHDR)).code };
            let depth = DEPTH.with(|d| {
                d.set(d.get() + 1);
                d.get()
            });
            MAX_DEPTH.with(|m| m.set(m.get().max(depth)));
            SEEN.with(|s| s.borrow_mut().push(code));
            if code < CHAIN {
                // A registration from inside the handler: a nested
                // reconcile with one new notice of its own.
                unsafe { deliver_container_notices(HWND::default(), vec![(hwnd, code + 1)]) };
            }
            DEPTH.with(|d| d.set(d.get() - 1));
            LRESULT(0)
        }

        unsafe {
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let class = WNDCLASSEXW {
                cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap_or(0),
                lpfnWndProc: Some(probe_proc),
                hInstance: instance.into(),
                lpszClassName: w!("CodePlusPlusTestNoticeProbe"),
                ..Default::default()
            };
            let _ = RegisterClassExW(&raw const class);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("CodePlusPlusTestNoticeProbe"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                None,
            )
            .expect("message-only probe window");

            deliver_container_notices(HWND::default(), vec![(hwnd, 1)]);
            let _ = DestroyWindow(hwnd);
        }

        assert_eq!(
            SEEN.with(|s| s.borrow().clone()),
            (1..=CHAIN).collect::<Vec<_>>(),
            "every chained notice must arrive, in the order it was raised"
        );
        assert_eq!(
            MAX_DEPTH.with(Cell::get),
            1,
            "a notice was delivered from inside another notice's handler"
        );
    }

    /// An unwind out of a drain must leave nothing behind: not the
    /// flag (every later notice would queue behind a drain that is
    /// gone) and not the queue (its entries are window handles, which
    /// Windows reuses).
    #[test]
    fn an_unwinding_drain_clears_the_flag_and_discards_the_queue() {
        let queued = HWND(0x5150 as *mut c_void);
        let caught = std::panic::catch_unwind(|| {
            let _drain = NoticeDelivery::enter().expect("no drain running on this thread");
            CONTAINER_NOTICES.with(|q| q.borrow_mut().push_back((HWND::default(), queued, 1)));
            panic!("simulated failure mid-drain");
        });
        assert!(caught.is_err(), "precondition: the closure unwound");
        assert!(
            !CONTAINER_NOTICES_DELIVERING.with(std::cell::Cell::get),
            "the drain flag survived the unwind"
        );
        assert!(
            CONTAINER_NOTICES.with(|q| q.borrow().is_empty()),
            "a queued window handle survived the unwind"
        );
    }

    /// The plugin's handler for `DMN_DOCK` / `DMN_FLOAT` / `DMN_CLOSE`
    /// runs inside the send, and may send `NPPM_*` straight back. That
    /// is answered only if no `WindowState` borrow is live across the
    /// send — the discipline the three sibling guards in `lib.rs` pin
    /// for notification delivery, and which holds here today by
    /// construction nobody had written down. The failure mode is the
    /// quiet one: move the send *into* the borrowing closure and it
    /// still compiles, reads naturally, and turns every `NPPM_*` from
    /// the handler into a decline — or into aliasing.
    ///
    /// So each check matches the statement's *end*, not merely the
    /// order of two names: a send placed inside the closure comes
    /// textually after the borrow begins, which an order check would
    /// accept.
    #[test]
    fn no_state_borrow_is_held_across_a_dock_notification() {
        use crate::plugin_reentry_guards::{code_only, fn_body, production_src, statement_end};

        let dock_src = include_str!("dock_panels.rs");
        let dock_src = &dock_src[..dock_src.find("#[cfg(test)]").expect("test module")];
        let apply = code_only(&fn_body(dock_src, "apply_dock_layout"));
        let start = apply
            .find("let notices = state_from_hwnd(main_hwnd)")
            .expect("the reconcile no longer takes the notices under a borrow");
        let send = apply
            .find("deliver_container_notices(main_hwnd")
            .expect("the reconcile no longer delivers the notices");
        assert!(
            send > statement_end(&apply, start),
            "the notices are delivered from inside the borrow that computed them"
        );
        let deliver = code_only(&fn_body(dock_src, "deliver_container_notices"));
        assert!(
            !deliver.contains("state_from_hwnd") && !deliver.contains("PluginCallGuard"),
            "the send loop takes a state borrow or arms the plugin guard"
        );

        let close = code_only(&fn_body(production_src(), "hide_plugin_panel"));
        let start = close
            .find("let target = unsafe { state_from_hwnd(main_hwnd) }")
            .expect("hide_plugin_panel no longer resolves its target under a borrow");
        let end = statement_end(&close, start);
        let send = close
            .find("SendMessageW(")
            .expect("hide_plugin_panel no longer sends DMN_CLOSE");
        assert!(send > end, "DMN_CLOSE is sent from inside the borrow");
        assert!(
            !close[end..send].contains("state_from_hwnd"),
            "a state borrow is taken between resolving the target and sending DMN_CLOSE"
        );
        assert!(
            !close.contains("PluginCallGuard"),
            "DMN_CLOSE's handler would have every NPPM_* declined"
        );
    }

    /// The whole send policy, through the real `DockEntry` record:
    /// one notice at registration, one per container change, none
    /// for a hide, a show, a re-activation or a same-side restack.
    #[test]
    fn container_notices_fire_on_registration_and_container_changes_only() {
        let panel =
            codepp_core::dock::intern_plugin_panel("cn.dll", "Notice Panel").expect("intern");
        let other =
            codepp_core::dock::intern_plugin_panel("cn2.dll", "Other Panel").expect("intern");
        let h = HWND(0x1234 as *mut c_void);
        let mut dialogs = vec![entry_for_test(panel, h)];
        let mut l = DockLayout::new();
        l.set_initial_side(panel, DockSide::Bottom);
        l.set_initial_side(other, DockSide::Bottom);

        // Registration, not yet shown: told where it will open.
        let n = container_notices(&l, &mut dialogs);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].0, h);
        assert_eq!(n[0].1, (3 << 16) | codepp_plugin_host::DMN_DOCK);
        // Nothing changed: nothing said.
        assert!(container_notices(&l, &mut dialogs).is_empty());

        // Shown where it was predicted to open, then joined by a
        // second panel and re-activated: all the same container.
        l.show(panel);
        l.show(other);
        l.activate(panel);
        assert!(container_notices(&l, &mut dialogs).is_empty());

        // A band of its own on the same side: same container.
        l.move_panel(panel, DropTarget::Side(DockSide::Bottom));
        assert!(container_notices(&l, &mut dialogs).is_empty());

        // Floated: DMN_FLOAT.
        l.move_panel(panel, DropTarget::Floating(DockRect::new(0, 0, 300, 200)));
        let n = container_notices(&l, &mut dialogs);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].1 & 0xFFFF, codepp_plugin_host::DMN_FLOAT);

        // Hidden and re-shown while floating: nothing.
        l.hide(panel);
        assert!(container_notices(&l, &mut dialogs).is_empty());
        l.show(panel);
        assert!(container_notices(&l, &mut dialogs).is_empty());

        // Docked on the left: DMN_DOCK with CONT_LEFT.
        l.move_panel(panel, DropTarget::Side(DockSide::Left));
        let n = container_notices(&l, &mut dialogs);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].1, codepp_plugin_host::DMN_DOCK);
    }

    #[test]
    fn caption_close_rect_pins_to_the_right_edge() {
        let r = caption_close_rect(300);
        assert_eq!(r.x, 300 - DOCK_CLOSE_W);
        assert_eq!(r.w, DOCK_CLOSE_W);
        assert!(r.contains(299, 10));
        assert!(!r.contains(200, 10));
        // Degenerate width clamps to the origin instead of going
        // negative.
        assert_eq!(caption_close_rect(10).x, 0);
    }

    #[test]
    fn tab_extents_active_carries_label_inactive_is_icon_only() {
        let widths = [90, 120];
        let extents = tab_extents(&widths, 0);
        let active_w = DOCK_TAB_PAD + DOCK_TAB_ICON_PX + DOCK_TAB_ICON_GAP + 90 + DOCK_TAB_PAD;
        let inactive_w = DOCK_TAB_PAD + DOCK_TAB_ICON_PX + DOCK_TAB_PAD;
        assert_eq!(extents, vec![(0, active_w), (active_w, inactive_w)]);
        // Flipping the active tab flips which one is wide.
        let flipped = tab_extents(&widths, 1);
        assert_eq!(flipped[0].1, inactive_w);
        assert_eq!(
            flipped[1].1,
            DOCK_TAB_PAD + DOCK_TAB_ICON_PX + DOCK_TAB_ICON_GAP + 120 + DOCK_TAB_PAD
        );
    }

    #[test]
    fn tear_off_size_is_a_third_of_the_window_floored_at_the_minimum() {
        assert_eq!(tear_off_size((1200, 900)), (400, 300));
        // A small window floors at the model minimum rather than a sliver.
        assert_eq!(tear_off_size((300, 240)), (MIN_FLOAT_W, MIN_FLOAT_H));
        assert_eq!(tear_off_size((0, 0)), (MIN_FLOAT_W, MIN_FLOAT_H));
    }

    #[test]
    fn tear_off_grab_keeps_the_pointer_fraction_along_the_caption() {
        // Pressed a quarter of the way along a 400-wide band → a quarter
        // of the way along the 200-wide float.
        assert_eq!(tear_off_grab(100, 400, 200), (50, DOCK_CAPTION_H / 2));
        // Beyond either end clamps rather than leaving the float behind.
        assert_eq!(tear_off_grab(-30, 400, 200).0, 0);
        assert_eq!(tear_off_grab(900, 400, 200).0, 200);
        // A degenerate source width does not divide by zero.
        assert_eq!(tear_off_grab(10, 0, 200).0, 200);
    }

    #[test]
    fn hit_tab_boundaries_are_half_open() {
        let extents = vec![(0, 40), (40, 100)];
        assert_eq!(hit_tab(&extents, 0), Some(0));
        assert_eq!(hit_tab(&extents, 39), Some(0));
        assert_eq!(hit_tab(&extents, 40), Some(1));
        assert_eq!(hit_tab(&extents, 139), Some(1));
        assert_eq!(hit_tab(&extents, 140), None);
        assert_eq!(hit_tab(&extents, -1), None);
    }

    #[test]
    fn group_content_rect_reserves_caption_and_conditional_tab_bar() {
        let solo = group_content_rect(300, 400, 1);
        assert_eq!(
            solo,
            DockRect::new(0, DOCK_CAPTION_H, 300, 400 - DOCK_CAPTION_H)
        );
        let tabbed = group_content_rect(300, 400, 2);
        assert_eq!(
            tabbed,
            DockRect::new(
                0,
                DOCK_CAPTION_H,
                300,
                400 - DOCK_CAPTION_H - DOCK_TAB_BAR_H
            )
        );
        // A crushed group clamps to zero rather than inverting.
        assert_eq!(group_content_rect(300, 10, 2).h, 0);
    }

    #[test]
    fn side_drag_sign_conventions() {
        // Dragging right (+x): grows Left, shrinks Right.
        assert_eq!(side_drag_size(DockSide::Left, 200, (30, 0)), 230);
        assert_eq!(side_drag_size(DockSide::Right, 200, (30, 0)), 170);
        // Dragging down (+y): grows Top, shrinks Bottom.
        assert_eq!(side_drag_size(DockSide::Top, 150, (0, 25)), 175);
        assert_eq!(side_drag_size(DockSide::Bottom, 150, (0, 25)), 125);
    }
}
