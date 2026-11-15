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
    compute_frame, resolve_drop, DockLayout, DockLocation, DockPanel, DockRect, DockSide,
    DragSubject, DropTarget, DropZones, MIN_FLOAT_H, MIN_FLOAT_W,
};
use std::ffi::c_void;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, ClientToScreen, CreateCompatibleDC, DeleteDC, DeleteObject, DrawTextW,
    EndPaint, FillRect, GetDC, GetObjectW, GetStockObject, GetSysColorBrush, GetTextExtentPoint32W,
    InvalidateRect, ReleaseDC, SelectObject, SetBkMode, AC_SRC_ALPHA, AC_SRC_OVER, BITMAP,
    BLENDFUNCTION, COLOR_3DFACE, COLOR_WINDOW, DEFAULT_GUI_FONT, DT_CENTER, DT_END_ELLIPSIS,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::NMHDR;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus,
};
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
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, KillTimer, PostMessageW, SetTimer, GA_PARENT, WM_DESTROY, WM_EXITSIZEMOVE,
    WM_MOVE, WM_PARENTNOTIFY,
};
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, IsWindowVisible, SendMessageW, WM_NOTIFY};

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
        // the plugin registers. Null, too, for a registration whose
        // window has left ([`registration_stands`]): that window is
        // the plugin's again, and nothing here may move or size it.
        DockPanel::Plugin(_) => state
            .dock_dialogs
            .iter()
            .find(|e| e.panel == panel && registration_stands(e.h_client))
            .map_or(HWND::default(), |e| e.h_client),
    }
}

// --- plugin panels whose window has left ---------------------------------------

/// Whether the registration a plugin made for `h_client` still stands:
/// the window exists, and is a direct child of one of the host's own
/// containers — a dock group, or the main window, where the content of
/// a panel that is not on screen is parked.
///
/// A plugin ends its registration by destroying the window, or by
/// re-parenting it out of the host's containers — `SetParent` into a
/// window of its own, or to the desktop. From then on the window is the
/// plugin's again: the `NPPM_DMM*` messages no longer find it, and
/// nothing in this module moves, sizes, shows, hides or re-parents it.
/// [`retire_departed`] drops the registration and closes its panel the
/// next time the host looks. Registering the window again, or another
/// under the same name, brings the panel back on the side it was on.
///
/// Read from Windows every time, never recorded. There is nothing to
/// record it from: a window is not told when a child is re-parented
/// away, and `WM_PARENTNOTIFY` for a destroyed one is sent only where
/// the child's own extended style allows it. So the fact is asked for
/// where it matters, which is three or four `user32` calls.
///
/// Before this, the reconciler did the opposite of what the GTK and
/// Cocoa backends do: its next pass found the window somewhere else and
/// took it back with a `SetParent` of its own, out of whatever window
/// the plugin had put it in. Measured with a probe plugin: Notepad++
/// 8.9.8.1 never takes such a window back — it only re-parents at
/// `NPPM_DMMREGASDCKDLG` — so a plugin written for it expects to keep
/// what it moved. DESIGN.md §7.4 has the measurement.
///
/// The parent is identified by window class, as [`find_main_hwnd`]
/// identifies the main window, so this needs no `WindowState` and can
/// be asked from inside any borrow. `GetAncestor(GA_PARENT)`, not
/// `GetParent`: a window made top-level again without `WS_POPUP` set
/// still has `WS_CHILD`, and what `GetParent` answers for it is not
/// worth relying on.
///
/// What this cannot see is a handle value Windows has given to another
/// window since the registered one was destroyed, the hazard every
/// `IsWindow` check on a kept handle has. The new window would have to
/// be a direct child of a host container too — which the host's own
/// windows are: its groups and splitters, the two built-in panels. So
/// a window of a class only the host registers never stands, as
/// registration refuses one (`is_host_own_window`): a stale
/// registration must not have the reconciler re-parent and size the
/// host's own chrome as a plugin's panel. A plugin's new window, or one
/// of the host's system-class controls, given the old value is the case
/// left.
pub(crate) fn registration_stands(h_client: HWND) -> bool {
    if h_client.is_invalid() {
        return false;
    }
    // SAFETY: every call takes any handle value. They read the handle
    // table, the window tree and a class name, and answer FALSE, null
    // or 0 for a handle that names no window.
    unsafe {
        IsWindow(Some(h_client)).as_bool()
            && !is_of_host_class(h_client)
            && is_host_container(GetAncestor(h_client, GA_PARENT))
    }
}

/// Whether `hwnd` is of a window class only the host registers. See
/// [`registration_stands`].
unsafe fn is_of_host_class(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    // SAFETY: the buffer's true length is passed; the call writes at
    // most that and answers 0 for a handle that names no window.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    usize::try_from(len).is_ok_and(|len| {
        crate::class_is_host_owned(&String::from_utf16_lossy(&buf[..len.min(buf.len())]))
    })
}

/// Whether `hwnd` is a window the host parents panel content to: the
/// main window or a dock group, by class. See [`registration_stands`].
unsafe fn is_host_container(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let mut buf = [0u16; 64];
    // SAFETY: the buffer's true length is passed; the call writes at
    // most that and answers 0 for a handle that names no window.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    let Ok(len) = usize::try_from(len) else {
        return false;
    };
    let name = &buf[..len.min(buf.len())];
    // SAFETY: both are `w!` literals, NUL-terminated and 'static.
    len > 0 && unsafe { name == MAIN_CLASS.as_wide() || name == DOCK_GROUP_CLASS.as_wide() }
}

/// What [`retire_departed`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Retired {
    /// How many registrations were dropped.
    pub registrations: usize,
    /// The panels closed because no registration stands for them now.
    pub closed: Vec<DockPanel>,
}

/// Drop every registration that no longer stands, and close the panels
/// they held. `stands` is [`registration_stands`]; a parameter so the
/// policy can be tested on handle values that name no window.
///
/// Closing is the honest outcome, and the one GTK and Cocoa reach:
/// nothing is left to show. The layout remembers the side the panel
/// was on, so a plugin that registers the window again, or a fresh one
/// under the same name, gets it back there. No `DMN_CLOSE`: the plugin took
/// the window away itself, as with `NPPM_DMMHIDE`.
///
/// A panel is left open when another registration stands for it, or
/// when it is `keep_open` — the panel `NPPM_DMMREGASDCKDLG` is about to
/// register a replacement for. A plugin that destroys its window and
/// registers a new one under the same name, a natural way to rebuild a
/// panel, keeps the panel on screen through it.
///
/// A parked panel is closed too, or it would be saved as open with
/// nothing registered for it.
///
/// Model and list only. It sends no message and moves no window, so it
/// runs under whatever borrow its caller holds; the caller reconciles
/// afterwards. Each dropped registration's tab icon, which is the
/// host's own bitmap, is deleted here.
pub(crate) fn retire_departed(
    dialogs: &mut Vec<crate::DockEntry>,
    layout: &mut DockLayout,
    keep_open: Option<DockPanel>,
    stands: impl Fn(HWND) -> bool,
) -> Retired {
    if dialogs.iter().all(|e| stands(e.h_client)) {
        return Retired::default();
    }
    let (live, departed): (Vec<crate::DockEntry>, Vec<crate::DockEntry>) = std::mem::take(dialogs)
        .into_iter()
        .partition(|e| stands(e.h_client));
    *dialogs = live;
    let mut closed: Vec<DockPanel> = Vec::new();
    for entry in &departed {
        if let Some(bmp) = entry.tab_icon {
            // SAFETY: the bitmap was made by `icon_to_tab_bitmap` at
            // registration and is owned by this entry alone, which is
            // being dropped.
            let _ = unsafe { DeleteObject(bmp.into()) };
        }
        let panel = entry.panel;
        if keep_open == Some(panel) || dialogs.iter().any(|e| e.panel == panel) {
            continue;
        }
        // Two ended registrations for one panel close it once: after
        // the first, it is neither open nor parked.
        if layout.is_visible(panel) || layout.is_parked(panel) {
            layout.hide(panel);
            closed.push(panel);
        }
    }
    Retired {
        registrations: departed.len(),
        closed,
    }
}

/// The one record of a retirement: nothing on screen says why a panel
/// closed.
pub(crate) fn log_retired(retired: &Retired) {
    for panel in &retired.closed {
        tracing::info!(
            panel = panel.persist_key(),
            "a plugin took its dock panel's window out of the host's containers, moving or \
             destroying it; closing the panel"
        );
    }
    if retired.registrations > 0 {
        tracing::debug!(
            registrations = retired.registrations,
            closed = retired.closed.len(),
            "dropped plugin dock registrations whose windows have left"
        );
    }
}

thread_local! {
    /// Set while a [`WM_APP_DOCK_SWEEP`] is posted and not yet handled,
    /// so a group that repaints many times asks once.
    ///
    /// [`WM_APP_DOCK_SWEEP`]: crate::WM_APP_DOCK_SWEEP
    static SWEEP_POSTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Ask the main window to look for registrations that no longer stand,
/// once control is back in a message loop
/// ([`sweep_departed_plugin_panels`]).
///
/// Posted, never done on the spot, because the two places that ask are
/// the wrong places to reconcile from. A group's `WM_PAINT` would have
/// the reconcile destroy the window being painted, when the departed
/// panel was the only one in it. And `WM_PARENTNOTIFY` arrives before
/// the child is destroyed, while it still reads as standing.
///
/// Nothing once the quit has begun: a plugin destroys its panels while
/// it shuts down, and the layout to save is the one the user had.
fn request_departure_sweep(main_hwnd: HWND) {
    if crate::quitting() || SWEEP_POSTED.with(|p| p.replace(true)) {
        return;
    }
    // SAFETY: `PostMessageW` takes any handle value and only queues.
    let posted = unsafe {
        PostMessageW(
            Some(main_hwnd),
            crate::WM_APP_DOCK_SWEEP,
            WPARAM(0),
            LPARAM(0),
        )
    };
    if posted.is_err() {
        // Nothing will clear the flag, so clear it here; the next
        // paint asks again.
        SWEEP_POSTED.with(|p| p.set(false));
    }
}

/// The main window's answer to [`request_departure_sweep`]: reconcile
/// if any registration no longer stands. The reconcile is what retires
/// it ([`apply_dock_layout`]'s first phase), so this decides nothing
/// itself.
///
/// # Safety
///
/// `main_hwnd` must be the main window. UI thread only, with no
/// `WindowState` borrow held.
pub(crate) unsafe fn sweep_departed_plugin_panels(main_hwnd: HWND) {
    // First, so nothing below can leave every later request waiting on
    // a sweep that never comes.
    SWEEP_POSTED.with(|p| p.set(false));
    if crate::quitting() {
        return;
    }
    // A state read declined here — the message dispatched by a loop
    // running inside a guarded plugin call — reads as nothing departed,
    // and the request is lost: the band stays empty until the group
    // paints again or anything else reconciles. Posting again from here
    // would spin that loop for as long as the guard is armed.
    let departed = unsafe { state_from_hwnd(main_hwnd) }.is_some_and(|state| {
        state
            .dock_dialogs
            .iter()
            .any(|e| !registration_stands(e.h_client))
    });
    if departed {
        unsafe { apply_dock_layout(main_hwnd) };
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
///
/// **One reconcile at a time on a thread.** Phases 2 to 4
/// ([`reconcile_tree`]) send messages to plugin windows, and a plugin
/// may answer one with an `NPPM_*` that changes the layout, which asks
/// for a reconcile from inside this one. Run there and then, the nested
/// pass would move the windows the outer one is in the middle of
/// moving — and its own messages could be answered the same way, with
/// nothing bounding the depth. So a call made while one is running only
/// notes that another pass is owed, and the running one makes it once
/// its own is done ([`run_reconcile`]).
///
/// The notifications of phase 6 are sent after the latch is released:
/// a handler that shows or hides a panel reconciles at once, as it
/// always has, and `drain_dock_notices` bounds that round trip with a
/// queue of its own.
pub(crate) unsafe fn apply_dock_layout(main_hwnd: HWND) {
    // SAFETY: the caller's contract, passed on to each pass.
    if run_reconcile(|| unsafe { reconcile_tree(main_hwnd) }).is_none() {
        return;
    }
    // Phase 6 (borrow, then none): tell each plugin what the change
    // means for its panels — a new container, a tab that came on
    // screen or went behind another, a panel laid out somewhere new —
    // and each newly registered one where its panel lives. Last, so a
    // plugin that reacts by querying the host or the window tree finds
    // both already settled: phase 4 has positioned every window, so a
    // panel has its new size by the time its plugin hears of it.
    //
    // SAFETY: the caller's contract — the main window, the UI thread,
    // no `WindowState` borrow held.
    unsafe { notify_plugin_panels(main_hwnd) };
}

/// Tell every plugin what the layout's latest change means for its
/// panels: record under a state borrow, then send with none held, so a
/// handler's `NPPM_*` is answered rather than declined.
///
/// Called at the end of every [`apply_dock_layout`], and by
/// [`check_plugin_panel_placements`] for the changes that bypass it — a
/// resize that changes where a group is laid out but not the model's
/// arrangement.
///
/// Nothing once the quit has begun. Every plugin has then heard
/// `NPPN_SHUTDOWN`, or is about to, and may have let go of what its
/// handlers need.
///
/// # Safety
///
/// `main_hwnd` must be the main window. UI thread only, with no
/// `WindowState` borrow held — the plugin's handler runs inside the
/// send and may call straight back into the host.
unsafe fn notify_plugin_panels(main_hwnd: HWND) {
    if crate::quitting() {
        return;
    }
    let notices = unsafe { state_from_hwnd(main_hwnd) }
        .map(|state| {
            let frame = dock_frame_now(main_hwnd, state);
            panel_notices(&state.dock_layout, frame.as_ref(), &mut state.dock_dialogs)
        })
        .unwrap_or_default();
    // SAFETY: the caller's contract; the borrow above ended with its
    // statement.
    unsafe { deliver_dock_notices(main_hwnd, notices) };
}

/// The carve of the dock area — everything between the toolbar's
/// bottom and the status bar's top — for a main window whose client
/// area is `width` × `height`, in its client coordinates.
///
/// The one carve: `layout_children` positions the bands and groups by
/// it, and [`panel_notices`] reads each docked panel's placement off
/// it, so a plugin is told `DMN_FLOATDROPPED` exactly when the layout
/// pass moved its panel. Two computations of it could differ by a
/// clamp or an inset, and a plugin would then be told of a move that
/// did not happen, or not told of one that did.
#[must_use]
pub(crate) fn dock_area_frame(
    layout: &DockLayout,
    toolbar_height: i32,
    width: i32,
    height: i32,
) -> codepp_core::dock::DockFrame {
    let mid = DockRect::new(
        0,
        toolbar_height,
        width.max(0),
        (height - STATUS_HEIGHT_PX - toolbar_height).max(0),
    );
    compute_frame(
        mid,
        layout,
        crate::MIN_SCINTILLA_WIDTH_PX,
        crate::MIN_SCINTILLA_HEIGHT_PX,
    )
}

/// [`dock_area_frame`] for the main window as it is now, or `None`
/// while its client area has no size — before its first layout, and
/// while it is minimized, when Windows reports a client area of
/// nothing. A docked panel has no placement to be told of then.
fn dock_frame_now(main_hwnd: HWND, state: &WindowState) -> Option<codepp_core::dock::DockFrame> {
    let mut rc = RECT::default();
    // SAFETY: `GetClientRect` takes any handle value and an
    // out-parameter that is live for the call; it sends no message.
    unsafe { GetClientRect(main_hwnd, &raw mut rc) }.ok()?;
    (rc.right > 0 && rc.bottom > 0).then(|| {
        dock_area_frame(
            &state.dock_layout,
            toolbar::toolbar_height_px(state.toolbar_bitmap_px),
            rc.right,
            rc.bottom,
        )
    })
}

/// The main window's timer that runs the placement check
/// ([`check_plugin_panel_placements`]). Not a small number, for the
/// reason `instance::FORWARDED_TIMER_ID` gives: a plugin is handed this
/// window and may set a timer of its own on it. "CPDK".
pub(crate) const PLACEMENT_TIMER_ID: usize = 0x4350_444B;
const _: () = assert!(
    PLACEMENT_TIMER_ID > 0xFFFF && PLACEMENT_TIMER_ID != crate::instance::FORWARDED_TIMER_ID,
    "the placement timer's id must not be one a plugin, or the host, is likely to use"
);

/// Ask the main window to check where the plugin panels are laid out,
/// once it has nothing else to do, so a panel whose group has moved or
/// changed size hears `DMN_FLOATDROPPED`
/// ([`check_plugin_panel_placements`]).
///
/// For the changes that do not go through [`apply_dock_layout`], which
/// notifies for itself: the main window resized, a band's splitter
/// dragged, the toolbar shown or hidden, a floating group's frame
/// dragged or the group moved. The places that learn of them —
/// `relayout_now`, [`on_group_size`], [`mirror_floating_rect`] — run
/// inside `WM_SIZE` and `WM_MOVE`, which Windows sends from wherever a
/// window was resized, possibly below a frame of a plugin's; so they
/// only set a timer, and calls made before it fires are absorbed by it:
/// setting a timer that is already set starts its wait again and makes
/// no second one. Nothing is lost if it fires late: the check reads the
/// layout as it is when it runs.
///
/// **A timer, not a posted message.** A plugin may answer
/// `DMN_FLOATDROPPED` by resizing the window its panel floats in, which
/// asks for another check, whose notice it answers the same way.
/// Windows hands out posted messages before input, so a check that
/// re-posted itself at every turn would keep the mouse and the keyboard
/// waiting for as long as the plugin kept at it. `WM_TIMER` is the last
/// thing a message loop is given, after input and painting, so each
/// round waits for the user's input to be dealt with, and for the
/// shortest interval Windows gives a timer. The loop is still the
/// plugin's to fix, as it is in Notepad++, where the same handler
/// recurses inside the container's relayout; the editor stays usable
/// meanwhile. GTK's idle and Cocoa's default-mode check give the same.
///
/// Nothing while no plugin panel is registered, which is nearly always,
/// so a resize costs a state read and no timer then. Nothing from
/// inside a reconcile either, whose own last phase tells the plugins,
/// and nothing once the quit has begun.
pub(crate) fn request_placement_check(main_hwnd: HWND) {
    if crate::quitting() || RECONCILING.with(std::cell::Cell::get) {
        return;
    }
    // A state read declined here — a resize dispatched inside a guarded
    // plugin call — asks for nothing, and the check it would have asked
    // for would have been declined too. The next layout pass asks again.
    //
    // SAFETY: `state_from_hwnd` takes any handle value; the borrow ends
    // with this statement.
    let plugin_panels =
        unsafe { state_from_hwnd(main_hwnd) }.is_some_and(|state| !state.dock_dialogs.is_empty());
    if plugin_panels {
        arm_placement_check(main_hwnd);
    }
}

/// Set the timer [`request_placement_check`] asks for. A call while it
/// is set starts its wait again; there is one timer of this id however
/// often this is called.
///
/// Nothing of the host's records that the timer is set. A record would
/// have to be right about a timer on a window every plugin is handed:
/// one that stopped it (`KillTimer` with this id) would leave the record
/// saying a check is on its way, and every later request would wait for
/// a check that never comes. Asking Windows each time costs one call a
/// layout pass, and only while a plugin panel is registered.
fn arm_placement_check(main_hwnd: HWND) {
    // An interval of 0 is the shortest Windows gives a timer. A timer
    // that cannot be set is left at that: the next layout pass asks
    // again.
    //
    // SAFETY: `SetTimer` takes any handle value and sends nothing.
    let _ = unsafe { SetTimer(Some(main_hwnd), PLACEMENT_TIMER_ID, 0, None) };
}

/// The main window's answer to [`request_placement_check`], when its
/// timer fires: tell each plugin panel that has been laid out somewhere
/// new.
///
/// Held back while the user is still resizing: inside the modal loop
/// Windows runs for a drag of a window's frame — the main window's or a
/// floating group's — while a band's splitter is being dragged, and
/// while the main window is minimized. A plugin that repositions a
/// window of its own on `DMN_FLOATDROPPED` should do it once, where the
/// panel ends up, not at every step on the way; and a minimized window
/// has no layout to report. The end of each asks again: `WM_EXITSIZEMOVE`
/// on either kind of window, the splitter's release, and the layout
/// pass a restore runs. So a resize is reported when it ends, as on
/// macOS, where Notepad++ reports every `WM_SIZE` step (read from its
/// source, not measured).
///
/// A check whose state read is declined — its timer fired inside a loop
/// a plugin runs during a guarded call — tells nothing, and nothing asks
/// again on its account: the panels hear of that layout at the next
/// layout pass or reconcile. Setting the timer again from here would
/// fire it in that same loop for as long as the guard is armed.
///
/// # Safety
///
/// `main_hwnd` must be the main window. UI thread only, with no
/// `WindowState` borrow held.
pub(crate) unsafe fn check_plugin_panel_placements(main_hwnd: HWND) {
    // The timer is one shot, stopped before anything else: so nothing
    // below leaves it ticking, into the teardown or through a resize,
    // and a change made while plugins are being told sets it anew.
    //
    // SAFETY: `KillTimer` takes any handle value and sends nothing.
    let _ = unsafe { KillTimer(Some(main_hwnd), PLACEMENT_TIMER_ID) };
    if crate::quitting() || unsafe { resize_under_way(main_hwnd) } {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe { notify_plugin_panels(main_hwnd) };
}

/// Whether the user is in the middle of a resize whose end will ask
/// for the placement check again. See [`check_plugin_panel_placements`].
unsafe fn resize_under_way(main_hwnd: HWND) -> bool {
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetGUIThreadInfo, IsIconic, GUITHREADINFO, GUI_INMOVESIZE,
    };
    // Asked of Windows rather than tracked between `WM_ENTERSIZEMOVE`
    // and `WM_EXITSIZEMOVE`: a flag of the host's own would stay set
    // for good if the second message never came, and every check after
    // that would be held back.
    let mut info = GUITHREADINFO {
        cbSize: u32::try_from(std::mem::size_of::<GUITHREADINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: `info` is a live out-parameter with its size set; the
    // call reads this thread's own input state. On failure the flags
    // stay clear, which reads as no drag.
    let in_move_size = unsafe { GetGUIThreadInfo(GetCurrentThreadId(), &raw mut info) }.is_ok()
        && info.flags.contains(GUI_INMOVESIZE);
    // SAFETY: `IsIconic` and `state_from_hwnd` take any handle value;
    // the borrow ends with its statement.
    in_move_size
        || unsafe { IsIconic(main_hwnd) }.as_bool()
        || unsafe { state_from_hwnd(main_hwnd) }.is_some_and(|state| state.dock_side_drag.is_some())
}

/// The most passes one [`apply_dock_layout`] makes before it stops
/// answering requests made from inside its own passes.
///
/// A second pass is ordinary: a plugin's window procedure answers a
/// message the first sent by showing or registering a panel. A plugin
/// that answers *every* pass that way — one that rebuilds its panel
/// each time the panel is shown, say — never settles, and each pass
/// creates and re-parents windows. Eight is far past anything that
/// settles and still a small, fixed amount of work.
const MAX_RECONCILE_PASSES: u32 = 8;

thread_local! {
    /// Set while [`run_reconcile`] is running passes on this thread.
    /// See [`Reconciling`].
    static RECONCILING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set when a reconcile was asked for while one was running.
    static RECONCILE_OWED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks a reconcile as running on this thread, so one asked for from
/// inside it is noted instead of run. RAII, like `NoticeDelivery`, so
/// the flag clears on the unwinding path too and one caught panic does
/// not leave every later reconcile noted and never run.
///
/// Before this, a plugin could recurse the reconciler, and nothing of
/// the host's bounded how deep. Phase 4 shows a panel's window, which
/// sends it `WM_SHOWWINDOW` with no borrow held; a handler that
/// destroys that window and registers a new one under the same name
/// marks the layout dirty, and the NPPM arm reconciled from inside the
/// handler — which showed the new window, whose handler did the same.
/// Each level is a whole reconcile and a trip through the main window
/// procedure. It became reachable when a replacement registration
/// stopped being refused; the security audit of that change found it,
/// and expected a stack overflow. Measured once with a probe plugin
/// that rebuilds its panel whenever it is shown, on Windows 11: the
/// nesting went 31 deep, then the plugin's `CreateWindowEx` returned no
/// window and the panel was left with none. Nothing crashed, but the
/// depth was whatever Windows allowed. With the latch the same plugin
/// gets [`MAX_RECONCILE_PASSES`] passes and a warning in the log.
struct Reconciling;

impl Reconciling {
    /// `None` when a reconcile is already running on this thread.
    fn enter() -> Option<Self> {
        if RECONCILING.with(|r| r.replace(true)) {
            return None;
        }
        Some(Self)
    }
}

impl Drop for Reconciling {
    fn drop(&mut self) {
        RECONCILING.with(|r| r.set(false));
    }
}

/// Run `pass` until no further one is owed, at most
/// [`MAX_RECONCILE_PASSES`] times, and answer how many ran. `None`
/// when a reconcile is already running on this thread: that one is
/// told another pass is owed, and nothing runs here.
///
/// Apart from [`apply_dock_layout`] so the bound can be tested without
/// a window tree.
fn run_reconcile(mut pass: impl FnMut()) -> Option<u32> {
    let Some(_running) = Reconciling::enter() else {
        RECONCILE_OWED.with(|o| o.set(true));
        return None;
    };
    let mut passes = 0;
    loop {
        RECONCILE_OWED.with(|o| o.set(false));
        pass();
        passes += 1;
        if !RECONCILE_OWED.with(std::cell::Cell::get) {
            return Some(passes);
        }
        if passes >= MAX_RECONCILE_PASSES {
            // What is still owed is dropped, not kept for later: the
            // next reconcile begins by clearing it, as every pass does.
            tracing::warn!(
                passes,
                "a plugin asks for a new dock layout every time its panel is laid out; \
                 stopped reconciling for it"
            );
            return Some(passes);
        }
    }
}

/// One pass of [`apply_dock_layout`]: phases 0 to 5.
///
/// # Safety
///
/// As [`apply_dock_layout`]: `main_hwnd` is the main window, on the UI
/// thread, with no `WindowState` borrow held.
unsafe fn reconcile_tree(main_hwnd: HWND) {
    unsafe {
        // The sweep a group asks for is this pass's first phase, so
        // whatever was asked for is being done. Clearing the latch
        // here also frees it if the posted message was lost — taken
        // off the queue by a loop that did not dispatch it — which
        // would otherwise silence every later request.
        SWEEP_POSTED.with(|p| p.set(false));

        // Phase 0 (borrow): drop the registrations whose windows have
        // left — destroyed, or re-parented out of the host's
        // containers by their plugin — and close their panels, before
        // anything below reads the model. This is the only place a
        // registration is retired outside `NPPM_DMMREGASDCKDLG`, so
        // every reconcile starts from registrations that stand.
        //
        // Not once the quit has begun: a plugin destroys its panels
        // as it shuts down, and the layout to save is the one the
        // user had.
        if !crate::quitting() {
            if let Some(state) = state_from_hwnd(main_hwnd) {
                let retired = retire_departed(
                    &mut state.dock_dialogs,
                    &mut state.dock_layout,
                    None,
                    registration_stands,
                );
                log_retired(&retired);
            }
        }

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
                        //
                        // Only registrations that stand: a window
                        // that has left is the plugin's again, and
                        // phase 0 retires none once the quit has
                        // begun.
                        let mut pairs = vec![
                            (DockPanel::Workspace, state.workspace_hwnd),
                            (DockPanel::DocMap, state.docmap_hwnd),
                        ];
                        pairs.extend(
                            state
                                .dock_dialogs
                                .iter()
                                .filter(|e| registration_stands(e.h_client))
                                .map(|e| (e.panel, e.h_client)),
                        );
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
                        // Asked again: the hide sent the window
                        // `WM_SHOWWINDOW`, and a plugin may have
                        // answered by taking it somewhere else.
                        if GetParent(panel_hwnd).unwrap_or_default() == gw.hwnd {
                            let _ = SetParent(panel_hwnd, Some(main_hwnd));
                        }
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
        //
        // A plugin's window is asked once more whether its
        // registration stands, right before each thing done to it.
        // Phases 2 to 4 send messages to plugin windows — every
        // `SetParent` and `ShowWindow` here does — whose handlers may
        // take a window out of the host's containers, and the
        // snapshot above would then have this pass take it back with
        // a `SetParent` of its own, or show or hide it in whatever
        // window the plugin had just put it in.
        let still_ours = |panel: DockPanel, hwnd: HWND| {
            !matches!(panel, DockPanel::Plugin(_)) || registration_stands(hwnd)
        };
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
                if !still_ours(*panel, *panel_hwnd) {
                    continue;
                }
                if GetParent(*panel_hwnd).unwrap_or_default() != ghwnd {
                    let _ = SetParent(*panel_hwnd, Some(ghwnd));
                    if !still_ours(*panel, *panel_hwnd) {
                        continue;
                    }
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
            if !layout.is_visible(panel) && still_ours(panel, panel_hwnd) {
                let _ = ShowWindow(panel_hwnd, SW_HIDE);
                if still_ours(panel, panel_hwnd)
                    && GetParent(panel_hwnd).unwrap_or_default() != main_hwnd
                {
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
    }
}

/// A `DMN_*` a dock reconcile owes a plugin about one of its panels,
/// waiting its turn in the delivery queue ([`drain_dock_notices`]).
///
/// It names the registration it was raised for by its serial
/// (`DockEntry::serial`) and carries no window handle. A notice can
/// wait behind a plugin's handler, the handler can destroy the panel's
/// window, and Windows gives a destroyed window's handle value to a new
/// one — which a plugin may register in turn. Found by handle, the old
/// registration's notice would reach the new one; found by serial, it
/// finds nothing and is dropped. Whom it goes to, and from which
/// window, is worked out when its turn comes ([`notice_target`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DockNotice {
    /// The panel the notice is about.
    pub panel: DockPanel,
    /// The registration it was raised for.
    pub serial: u64,
    /// The `nmhdr.code`: `MAKELONG(DMN_DOCK or DMN_FLOAT, container)`,
    /// or a bare `DMN_SWITCHIN`, `DMN_SWITCHOFF` or `DMN_FLOATDROPPED`.
    pub code: u32,
}

thread_local! {
    /// The `DockEntry::serial` the next registration takes.
    static NEXT_REGISTRATION: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// The serial for a registration being made: each call answers the
/// next number, so no two registrations of a session share one, and a
/// queued [`DockNotice`] can only ever find the registration it was
/// raised for.
pub(crate) fn next_registration_serial() -> u64 {
    NEXT_REGISTRATION.with(|next| next.replace(next.get() + 1))
}

/// Bring each registered plugin panel's record up to date and return,
/// in the order they go out, the `DMN_*` its plugin is owed for the
/// change: `DMN_DOCK` / `DMN_FLOAT` for a new container — every panel
/// whose plugin has been told nothing yet included, which is how a
/// freshly registered panel gets upstream's registration-time
/// notification — `DMN_SWITCHIN` / `DMN_SWITCHOFF` for a tab that came
/// on screen or went behind another, and `DMN_FLOATDROPPED` for a panel
/// laid out somewhere new. When each is owed, and the order, are
/// `codepp_plugin_host::docking`'s to decide — `PanelTold::update` and
/// `delivery_order` — so this backend sends what GTK's and Cocoa's send
/// for the same change.
///
/// Recording happens here, under the caller's borrow, before
/// [`drain_dock_notices`] sends anything, and that order is what stops
/// a change being told twice: a plugin's handler may show or hide a
/// panel, which reconciles again from inside the delivery, and the
/// nested pass must find the change already recorded. A source scan
/// pins the order.
///
/// Recording first does **not** bound the round trip on its own, and
/// an earlier version of this comment claimed it did. A handler that
/// *registers a new panel* gives the nested pass something genuinely
/// new — an entry nothing has been told about — so it produces a
/// notice of its own, whose handler may register another; and handlers
/// that keep reversing the layout — each bringing its own tab back to
/// the front when told it went behind — owe each other a notice at
/// every turn. The queue in `drain_dock_notices`, and its cap, are what
/// close both.
///
/// `frame` is [`dock_area_frame`] for the main window as it is laid
/// out now, or `None` while it has no size; before it has one, no
/// docked panel has a placement to be told of.
pub(crate) fn panel_notices(
    layout: &DockLayout,
    frame: Option<&codepp_core::dock::DockFrame>,
    dialogs: &mut [crate::DockEntry],
) -> Vec<DockNotice> {
    // Keyed by the registration, copied out of it, so turning the
    // ordered codes into notices needs nothing looked up again.
    let owed: Vec<((DockPanel, u64), codepp_plugin_host::docking::Owed)> = dialogs
        .iter_mut()
        .map(|entry| {
            let owed = entry.told.update(layout, frame, entry.panel);
            ((entry.panel, entry.serial), owed)
        })
        .collect();
    codepp_plugin_host::docking::delivery_order(&owed)
        .into_iter()
        .map(|((panel, serial), code)| DockNotice {
            panel,
            serial,
            code,
        })
        .collect()
}

/// Whom a queued notice goes to when its turn comes, and which window
/// it comes from: `(h_client, hwndFrom)`. `None` for a notice that is
/// no longer worth sending, since a plugin's handler for an earlier one
/// may have changed things while this one waited:
///
/// - **The registration it was raised for must still stand** — found
///   by its serial, and its window still in the host's containers
///   (`stands` is [`registration_stands`]; a parameter so the rule can
///   be tested on handle values that name no window).
/// - **What it says must still hold of the model**, by the shared rule
///   in `codepp_plugin_host::docking::still_applies`: a `DMN_SWITCHIN`
///   only for a panel still in front, a `DMN_FLOATDROPPED` only for one
///   still in a group, anything else as queued.
/// - **A notice from the container needs one.** A `DMN_SWITCHOFF` whose
///   panel has been closed since has no group to come from, and is
///   dropped: Notepad++ sends a closed panel none either.
///
/// The record is already written by the time a notice is queued, so a
/// dropped one loses nothing that could still be delivered.
///
/// **`hwndFrom` follows Notepad++, which sends the two families from
/// two windows.** `DMN_DOCK` and `DMN_FLOAT` come from the main window
/// (measured, like `DMN_CLOSE`: DESIGN.md §7.4), which is the only
/// sender its docking-dialog template accepts. `DMN_SWITCHIN`,
/// `DMN_SWITCHOFF` and `DMN_FLOATDROPPED` come from the panel's
/// container — measured as tabs switch, and read from its source for
/// the rest — which here is the group window the panel's `h_client` is
/// a child of. A group window is remade when its group floats or
/// docks, so the handle is looked up now and never kept in the queue.
fn notice_target(
    dialogs: &[crate::DockEntry],
    layout: &DockLayout,
    groups: &[DockGroupWindow],
    main_hwnd: HWND,
    notice: &DockNotice,
    stands: impl Fn(HWND) -> bool,
) -> Option<(HWND, HWND)> {
    let entry = dialogs
        .iter()
        .find(|e| e.serial == notice.serial && stands(e.h_client))?;
    if !codepp_plugin_host::docking::still_applies(layout, entry.panel, notice.code) {
        return None;
    }
    let from = match notice.code & 0xFFFF {
        codepp_plugin_host::DMN_DOCK | codepp_plugin_host::DMN_FLOAT => main_hwnd,
        _ => {
            let group = layout.group_of(entry.panel)?.id;
            groups
                .iter()
                .find(|gw| gw.id == group && !gw.hwnd.is_invalid())?
                .hwnd
        }
    };
    Some((entry.h_client, from))
}

/// Send each notice a dock reconcile owes — `DMN_DOCK` / `DMN_FLOAT`,
/// `DMN_SWITCHIN` / `DMN_SWITCHOFF` and `DMN_FLOATDROPPED`, in the
/// order [`panel_notices`] gave them — through the one queue
/// ([`drain_dock_notices`]).
///
/// # Safety
///
/// `main_hwnd` must be the main window HWND. UI thread only, with no
/// `WindowState` borrow held — the plugin's handler runs inside the
/// send and may call straight back into the host.
unsafe fn deliver_dock_notices(main_hwnd: HWND, notices: Vec<DockNotice>) {
    // SAFETY: the caller's contract, passed on to each send.
    drain_dock_notices(notices, |notice| unsafe {
        send_dock_notice(main_hwnd, notice);
    });
}

/// Queue `notices` and, unless a delivery is already running on this
/// thread, hand each queued notice to `send` in order until the queue
/// is empty. `send` is [`send_dock_notice`]; a parameter so the queue's
/// rules can be tested without a window tree.
///
/// **Notices raised while one is being delivered are queued, not
/// sent.** The plugin's handler runs inside the send with no borrow
/// held, so it may send `NPPM_*` back — including
/// `NPPM_DMMREGASDCKDLG` for a panel nothing has been told about, which
/// reconciles again from inside this loop and raises a notice of its
/// own. Sent there and then, that notice's handler could do the same,
/// nesting a full window-procedure round trip per link until the
/// registration cap or Windows' own limit on nested callbacks cut it
/// off. So a call made while a delivery is already running on this
/// thread only appends to the queue and returns, and the outermost call
/// drains it in order. The window-procedure nesting this path can cause
/// is one level, whatever the plugin does.
///
/// Unlike `DmnCloseGuard`, which *skips* a nested `DMN_CLOSE`, nothing
/// that is still true is dropped here: the record is already written by
/// the time a notice is queued, so a skipped notice would never be sent
/// at all, and a plugin registering from inside a handler would never
/// learn where its panel is. The cost is timing, and only in that
/// nested case: the panel registered inside a handler is told its
/// container after that handler returns, rather than before its
/// `NPPM_DMMREGASDCKDLG` returns. FIFO order also means that if a
/// nested reconcile records a newer container for a panel whose older
/// notice is still waiting, the plugin receives both in record order
/// and ends on the latest.
///
/// The queue bounds depth, and the shared cap,
/// `codepp_plugin_host::docking::MAX_NOTICES_PER_DELIVERY`, bounds the
/// rest: one outermost delivery works through at most that many
/// notices, sent or found stale, and drops what is still queued; and
/// the queue never holds more than that many ([`queue_dock_notices`]),
/// so a handler that keeps raising notices while one is delivered
/// cannot grow it either. Without the cap, plugins whose handlers keep
/// reversing the layout would hold this loop, and the UI thread with
/// it, for as long as they kept at it. What is dropped is reported in
/// one warning when the delivery ends.
///
/// Nothing once the quit has begun, and a handler may begin it — by
/// closing the main window: the rest of the queue is then dropped
/// unsent. Every plugin is about to hear `NPPN_SHUTDOWN`, or has, and
/// hears nothing of the dock after that.
fn drain_dock_notices(notices: Vec<DockNotice>, mut send: impl FnMut(DockNotice)) {
    if crate::quitting() {
        return;
    }
    queue_dock_notices(notices);
    // A delivery is already running further up this thread's stack:
    // it will send what was just queued once its current handler
    // returns.
    let Some(_delivering) = NoticeDelivery::enter() else {
        return;
    };
    let mut taken = 0usize;
    // Popped one at a time, with the queue's borrow released before
    // the send — the handler may queue more, which needs the borrow.
    while let Some(notice) = DOCK_NOTICES.with(|q| q.borrow_mut().pop_front()) {
        taken += 1;
        if taken > codepp_plugin_host::docking::MAX_NOTICES_PER_DELIVERY {
            let left = DOCK_NOTICES.with(|q| {
                let mut q = q.borrow_mut();
                let left = q.len();
                q.clear();
                left
            });
            DOCK_NOTICES_DROPPED.with(|dropped| dropped.set(dropped.get() + 1 + left));
            break;
        }
        if crate::quitting() {
            DOCK_NOTICES.with(|q| q.borrow_mut().clear());
            break;
        }
        send(notice);
    }
    let dropped = DOCK_NOTICES_DROPPED.with(|dropped| dropped.replace(0));
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "plugins keep changing the dock layout from their DMN_* handlers; \
             dropped the notifications past one delivery's cap"
        );
    }
}

/// Append `notices` to the queue, keeping it no longer than one
/// delivery may send. Anything past that could never go out — a
/// delivery drops whatever is still queued once it reaches the cap —
/// and refusing it here is what bounds the queue while a plugin's
/// handler keeps raising notices from inside a delivery. What is
/// refused is counted for the delivery's warning.
fn queue_dock_notices(notices: Vec<DockNotice>) {
    let refused = DOCK_NOTICES.with(|q| {
        let mut q = q.borrow_mut();
        let room = codepp_plugin_host::docking::MAX_NOTICES_PER_DELIVERY.saturating_sub(q.len());
        let refused = notices.len().saturating_sub(room);
        q.extend(notices.into_iter().take(room));
        refused
    });
    if refused > 0 {
        DOCK_NOTICES_DROPPED.with(|dropped| dropped.set(dropped.get() + refused));
    }
}

/// Send one queued notice, as upstream sends it: a plain `WM_NOTIFY`
/// to the plugin's own `h_client`, `wParam` 0, `nmhdr.idFrom` 0, and
/// `nmhdr.hwndFrom` the main window for `DMN_DOCK` / `DMN_FLOAT` and
/// the panel's group window for the other three ([`notice_target`]) —
/// if the notice is still worth sending when its turn has come.
///
/// The target is resolved under a state borrow that ends before the
/// send: the plugin's handler runs inside `SendMessageW` and may send
/// `NPPM_*` straight back, which is answered only if nothing is
/// borrowed. A state read declined here drops the notice.
///
/// Sends nothing once the quit has begun. Every `DMN_DOCK`,
/// `DMN_FLOAT`, `DMN_SWITCHIN`, `DMN_SWITCHOFF` and `DMN_FLOATDROPPED`
/// goes out through here, so that holds for any caller, one added
/// later included. The callers stop sooner — a reconcile raises
/// nothing, a delivery drops its queue — and this is the backstop
/// behind them.
///
/// # Safety
///
/// `main_hwnd` must be the main window HWND. UI thread only, with no
/// `WindowState` borrow held.
unsafe fn send_dock_notice(main_hwnd: HWND, notice: DockNotice) {
    if crate::quitting() {
        return;
    }
    let target = unsafe { state_from_hwnd(main_hwnd) }.and_then(|state| {
        notice_target(
            &state.dock_dialogs,
            &state.dock_layout,
            &state.dock_groups,
            main_hwnd,
            &notice,
            registration_stands,
        )
    });
    let Some((h_client, from)) = target else {
        return;
    };
    // The one record of what a plugin was told about its panel; a
    // plugin reports its reaction only through its own state, so this
    // is what connects the two when one misbehaves.
    tracing::debug!(
        panel = notice.panel.persist_key(),
        dmn = codepp_plugin_host::docking::dmn_name(notice.code),
        container = notice.code >> 16,
        "dock panel notification"
    );
    let nmhdr = NMHDR {
        hwndFrom: from,
        idFrom: 0,
        code: notice.code,
    };
    // SAFETY: `nmhdr` outlives the synchronous send, and no state
    // borrow is live across it.
    unsafe {
        SendMessageW(
            h_client,
            WM_NOTIFY,
            Some(WPARAM(0)),
            Some(LPARAM(&raw const nmhdr as isize)),
        );
    }
}

/// Whether a delivery of dock notices is running on this thread, so
/// that whatever asks is running inside a plugin's handler for one. A
/// registration refreshed from there is not told its container again:
/// the notice it would raise is queued behind the handler, whose next
/// run would raise another, and the drain would not end before its cap
/// while the plugin kept at it.
pub(crate) fn delivering_dock_notices() -> bool {
    DOCK_NOTICES_DELIVERING.with(std::cell::Cell::get)
}

thread_local! {
    /// Notices waiting to be sent. See [`drain_dock_notices`].
    static DOCK_NOTICES: std::cell::RefCell<std::collections::VecDeque<DockNotice>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    /// Set while [`drain_dock_notices`] is draining on this thread. See
    /// [`NoticeDelivery`].
    static DOCK_NOTICES_DELIVERING: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    /// Notices dropped since the last warning: refused because the
    /// queue already held as many as one delivery may send, or still
    /// queued when a delivery reached its cap. See
    /// [`drain_dock_notices`].
    static DOCK_NOTICES_DROPPED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Marks a drain of dock notices as running on this thread, so a
/// nested call queues instead of sending. RAII for the same reason as
/// `DmnCloseGuard` and `DrainFreeze`: the flag must clear on the
/// unwinding path too, or one caught panic leaves every later notice
/// queued behind a drain that no longer exists.
///
/// **On that path it also discards whatever is still queued**, which
/// is where this differs from `DrainFreeze`: that guard defers channel
/// work that is safe to replay whenever, while a notice says what was
/// true of the layout when it was raised. Left behind, it would go out
/// with the next delivery, whenever that is — after something has
/// already gone wrong, and ahead of whatever that delivery was for.
/// Dropping it costs that plugin one notice. How an unwind could reach
/// here at all is narrow. Nothing reached *through* the send can
/// produce one: every window procedure — the plugin's, and the host's
/// own when a handler sends `NPPM_*` back — is a plain
/// `extern "system"` function, and a panic inside one aborts at that
/// function's own boundary rather than unwinding back out of
/// `SendMessageW`. (That is also why no `catch_unwind` sits around the
/// send: it could never catch anything. It is not because the host's
/// procedures guard themselves; most do not.) So an unwind would have
/// to start in the drain itself, and release builds abort on panic
/// regardless.
struct NoticeDelivery;

impl NoticeDelivery {
    /// `None` when a drain is already running on this thread.
    fn enter() -> Option<Self> {
        if DOCK_NOTICES_DELIVERING.with(std::cell::Cell::get) {
            return None;
        }
        DOCK_NOTICES_DELIVERING.with(|f| f.set(true));
        Some(Self)
    }
}

impl Drop for NoticeDelivery {
    fn drop(&mut self) {
        DOCK_NOTICES_DELIVERING.with(|f| f.set(false));
        // The normal path leaves the queue empty — the drain ends when
        // `pop_front` does, or clears what is left itself — so this
        // only ever discards on an unwind. `try_` throughout because a
        // destructor must not panic and a plain `borrow_mut` would if
        // an unwind ever began with a borrow of the queue live. None
        // does today — every borrow in `drain_dock_notices` ends before
        // the next statement — but that is a property of the loop's
        // current shape, not of anything this destructor can check.
        if std::thread::panicking() {
            let _ = DOCK_NOTICES.try_with(|q| {
                if let Ok(mut q) = q.try_borrow_mut() {
                    q.clear();
                }
            });
        }
    }
}

thread_local! {
    /// The plugin panel a `NPPM_DMMSHOW` or `NPPM_DMMVIEWOTHERTAB` asked
    /// to give keyboard focus — see [`request_panel_focus`].
    static PANEL_FOCUS: std::cell::Cell<Option<DockPanel>> = const { std::cell::Cell::new(None) };
    /// Set while [`apply_requested_panel_focus`] is moving focus on this
    /// thread. See [`PanelFocusing`].
    static PANEL_FOCUSING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks [`apply_requested_panel_focus`] as moving focus on this
/// thread, so a request made from inside the move is dropped rather
/// than applied there.
///
/// `SetFocus` sends `WM_SETFOCUS` to the panel synchronously, and a
/// plugin may answer it with `NPPM_DMMSHOW` for that same panel — to
/// make sure it is in front, say — after moving focus to one of its
/// own controls. Applied in turn, that request would move focus back to
/// the panel, which sends `WM_SETFOCUS` again, with nothing bounding
/// the depth: the `DMN_CLOSE` recursion `DmnCloseGuard` latches, in
/// another message. In that case the panel the outer move is focusing
/// is the one the plugin asked for anyway; a handler that shows a
/// different panel gets it shown but not focused. RAII so the flag
/// clears on the unwinding path too.
struct PanelFocusing;

impl PanelFocusing {
    /// `None` when a move is already under way on this thread.
    fn enter() -> Option<Self> {
        if PANEL_FOCUSING.with(std::cell::Cell::get) {
            return None;
        }
        PANEL_FOCUSING.with(|f| f.set(true));
        Some(Self)
    }
}

impl Drop for PanelFocusing {
    fn drop(&mut self) {
        PANEL_FOCUSING.with(|f| f.set(false));
    }
}

/// Ask for `panel`'s content to take keyboard focus once
/// [`apply_dock_layout`] has put it on screen.
///
/// Notepad++ moves focus to a panel's `hClient` whenever a plugin shows
/// it with `NPPM_DMMSHOW` or brings it forward with
/// `NPPM_DMMVIEWOTHERTAB`, even when it is already in front — measured
/// with a probe plugin in Notepad++ 8.9.8.1 (DESIGN.md §7.4). A plugin
/// whose command opens a console expects what the user types next to
/// land in it. The handlers run inside the NPPM dispatch, before the
/// window tree has caught up and while the panel may still be hidden —
/// `NPPM_DMMVIEWOTHERTAB` shows a hidden one — so they only record the
/// request; the `wnd_proc` arm applies it with
/// [`apply_requested_panel_focus`] after its reconcile.
///
/// One request is held at a time, and the last one wins: a plugin that
/// shows two panels in one dispatch focuses the second, which is where
/// Notepad++, moving focus at each show, leaves it too. Every NPPM arm
/// applies whatever is held when its dispatch ends, so one nested inside
/// another — a plugin answering a notification the outer dispatch set
/// off — may apply the outer request; it does so after its own
/// reconcile, so the panel is on screen by then.
///
/// One case loses the request. A show sent from a handler that a
/// reconcile pass itself called — a plugin window's `WM_SHOWWINDOW` —
/// has its reconcile put off until that pass is over
/// ([`apply_dock_layout`]), so its arm finds the panel not yet on screen
/// and drops the request. Keeping it for later instead was considered:
/// the pass may have been started by a drag or a sweep, with no NPPM
/// arm to apply it afterwards, and the request would then move focus at
/// whatever unrelated message a plugin sent next, perhaps seconds later
/// and in the middle of the user's typing. A lost focus is the smaller
/// surprise.
///
/// The panel is recorded rather than its window, so a request that
/// outlives its registration finds nothing instead of a reused handle.
pub(crate) fn request_panel_focus(panel: DockPanel) {
    PANEL_FOCUS.with(|f| f.set(Some(panel)));
}

/// Give keyboard focus to the panel [`request_panel_focus`] named, if
/// it is still registered and its content is on screen, and forget the
/// request either way. A request made while this is moving focus is
/// dropped — see [`PanelFocusing`].
///
/// # Safety
///
/// `main_hwnd` must be the main window. UI thread only, with no
/// `WindowState` borrow live: this takes one, and `SetFocus` sends
/// `WM_KILLFOCUS` / `WM_SETFOCUS` synchronously, which reach window
/// procedures that take their own.
pub(crate) unsafe fn apply_requested_panel_focus(main_hwnd: HWND) {
    let Some(panel) = PANEL_FOCUS.with(std::cell::Cell::take) else {
        return;
    };
    let Some(_focusing) = PanelFocusing::enter() else {
        tracing::debug!(
            panel = panel.persist_key(),
            "panel focus requested during a focus move; dropped"
        );
        return;
    };
    let client = unsafe { state_from_hwnd(main_hwnd) }.and_then(|state| {
        state
            .dock_dialogs
            .iter()
            .find(|e| e.panel == panel && registration_stands(e.h_client))
            .map(|e| e.h_client)
    });
    let Some(client) = client else {
        tracing::debug!(
            panel = panel.persist_key(),
            "panel focus requested for a panel no longer registered; dropped"
        );
        return;
    };
    if unsafe { IsWindowVisible(client) }.as_bool() {
        let _ = unsafe { SetFocus(Some(client)) };
    } else {
        tracing::debug!(
            panel = panel.persist_key(),
            "panel focus requested for a panel not on screen; dropped"
        );
    }
}

/// The window with keyboard focus on this thread, for [`return_focus`].
pub(crate) fn focus_now() -> HWND {
    // SAFETY: `GetFocus` takes no arguments and reads only this
    // thread's input state.
    unsafe { GetFocus() }
}

/// Put keyboard focus back on `before` if a plugin load pass moved it.
///
/// Notepad++ loads its plugins at startup and restores their panels by
/// running each one's command, which shows the panel and moves focus
/// there; then it activates the session's buffer, which puts focus back
/// on the editor, so by `NPPN_READY` it is there again — measured with a
/// probe plugin in Notepad++ 8.9.8.1. Code++ loads a plugin when it is
/// first needed, so a pass can run in the middle of a session, and
/// without this a restored panel's command, or a plugin showing its
/// panel from `NPPN_TBMODIFICATION`, would leave what the user types
/// next going into a panel they never asked for. A null `before` does
/// nothing; the load pass passes the editor instead when no window of
/// the thread had focus, which is so while Code++ is in the background.
/// At startup `run` also focuses the editor once the pass is over.
///
/// A `before` that has been destroyed, or hidden, during the pass is
/// left alone too, and focus stays where the pass put it: `SetFocus`
/// focuses a hidden window as readily as a shown one, and what the user
/// types next would go nowhere they could see.
///
/// # Safety
///
/// UI thread only, with no `WindowState` borrow live: `SetFocus` sends
/// `WM_KILLFOCUS` / `WM_SETFOCUS` synchronously.
pub(crate) unsafe fn return_focus(before: HWND) {
    if let Some(window) = focus_to_return(before, window_is_shown(before), focus_now()) {
        // SAFETY: the caller is on the UI thread with no `WindowState`
        // borrow live, as `# Safety` requires.
        let _ = unsafe { SetFocus(Some(window)) };
    }
}

/// Whether `window` is a window that is not destroyed and reads as
/// shown: it and every ancestor, the desktop window included, have
/// `WS_VISIBLE`.
fn window_is_shown(window: HWND) -> bool {
    // SAFETY: `IsWindow` and `IsWindowVisible` take any handle value:
    // they only read the handle table and window styles, and answer
    // FALSE for a handle that names no window.
    !window.is_invalid()
        && unsafe { IsWindow(Some(window)) }.as_bool()
        && unsafe { IsWindowVisible(window) }.as_bool()
}

/// The window [`return_focus`] gives focus to when `now` has it:
/// `before`, if it is shown (`before_shown`, from [`window_is_shown`])
/// and does not have focus already.
///
/// Apart from the queries so a test can check the decision without
/// windows. On a desktop whose desktop window does not read as shown,
/// presumably the Windows CI runner's, no window reads as shown, so a
/// test of the queries alone cannot reach the case that gives focus
/// back there.
fn focus_to_return(before: HWND, before_shown: bool, now: HWND) -> Option<HWND> {
    (before_shown && now != before).then_some(before)
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

/// Whether a tab drag that ends now moves `panel`: only while the
/// panel is still open in a group.
///
/// A drag lasts as long as the button is held, and a plugin can close
/// the dragged panel meanwhile — `NPPM_DMMHIDE` from a timer, or its
/// window destroyed or taken away, which retires the registration
/// ([`retire_departed`]). `DockLayout::move_panel` reopens a closed
/// panel at its target, so without this the drop would put back what
/// the plugin had just closed; a retired panel would come back as a
/// group with nothing registered for it, which nothing would ever
/// close again.
#[must_use]
fn drop_moves_panel(layout: &DockLayout, panel: DockPanel) -> bool {
    layout.is_visible(panel)
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
            WM_MOVE => {
                // A floating group has moved without changing size —
                // a resize arrives as `WM_SIZE` as well. Docked groups
                // are moved by every layout pass, which reports for
                // itself.
                if let Some((main, snap)) = group_snapshot(hwnd) {
                    if snap.floating {
                        mirror_floating_rect(main, hwnd);
                    }
                }
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
            WM_EXITSIZEMOVE => {
                // The user let go of a floating group's frame. The
                // checks its `WM_SIZE`s asked for were held back while
                // the drag went on, so the resize is reported once,
                // here.
                if let Some(main) = find_main_hwnd(hwnd) {
                    request_placement_check(main);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_PARENTNOTIFY => {
                // A child of this group is about to be destroyed. Its
                // children are panel content, so if it is a plugin
                // panel's, the registration ends with it; look once
                // the destroy is over, since the window still exists
                // right now. The low word is the event — clicks arrive
                // here too — and `lParam` the window: a control deeper
                // inside a plugin's panel is passed up as well, and
                // destroying one of those ends nothing.
                if (wparam.0 & 0xFFFF) as u32 == WM_DESTROY
                    && GetAncestor(HWND(lparam.0 as *mut c_void), GA_PARENT) == hwnd
                {
                    if let Some(main) = find_main_hwnd(hwnd) {
                        request_departure_sweep(main);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
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

/// Position every panel's content inside the group, mirror a floating
/// resize back into the model, and repaint the chrome.
///
/// Every panel's, the ones behind the front tab included: all of a
/// group's tabs share the one content slot, and a plugin told its panel
/// was laid out anew (`DMN_FLOATDROPPED`, which goes to every panel of
/// a group that moved or changed size) must find its window at the new
/// size whether or not it is the tab in front. Notepad++ sizes every
/// tab's window before it sends that notice (read from its source).
/// Until the notice existed only the front tab's content was sized
/// here, and a tab got its size when it came to the front.
unsafe fn on_group_size(ghwnd: HWND) {
    unsafe {
        let Some((main, snap)) = group_snapshot(ghwnd) else {
            return;
        };
        // Snapshot the content HWNDs under the same borrow, then
        // drop it before MoveWindow (which cascades WM_SIZE into
        // the panel procs — the standard aliasing discipline).
        let contents = {
            let Some(state) = state_from_hwnd(main) else {
                return;
            };
            let contents: Vec<HWND> = snap
                .panels
                .iter()
                .map(|p| panel_content_hwnd(state, *p))
                .collect();
            contents
        };
        let mut rc = RECT::default();
        let _ = GetClientRect(ghwnd, &raw mut rc);
        let content_rc = group_content_rect(rc.right, rc.bottom, snap.panels.len());
        for content in contents {
            // Only content that is this group's child: a panel the
            // reconciler has not parented in yet is positioned by the
            // call it makes once it has, and a plugin's window that
            // has left is the plugin's again. Asked for each one in
            // turn, since moving a window sends it messages its plugin
            // may answer by taking another away.
            if !content.0.is_null() && GetParent(content).unwrap_or_default() == ghwnd {
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
        if snap.floating {
            mirror_floating_rect(main, ghwnd);
        }
        let _ = InvalidateRect(Some(ghwnd), None, true);
    }
}

/// Keep the model's rectangle for a floating group the one its window
/// has, and ask for the placement check: a plugin panel in the group
/// has a new place, and neither a drag of the frame nor a move goes
/// through a reconcile that would tell its plugin.
///
/// Called for a resize ([`on_group_size`]) and for a move (`WM_MOVE`).
/// A float has no title bar of the system's to drag, and Code++'s own
/// caption drag moves it through the model; but the system moves
/// windows too — a display that goes away, a window-arranging tool —
/// and until `WM_MOVE` was mirrored such a move left the model, and so
/// the saved session and what plugins had been told, at the old place.
///
/// # Safety
///
/// `ghwnd` must be a floating group's window and `main` the main
/// window. UI thread only, with no `WindowState` borrow held.
unsafe fn mirror_floating_rect(main: HWND, ghwnd: HWND) {
    let rect = rect_of(ghwnd);
    unsafe {
        let id = GetWindowLongPtrW(ghwnd, GWLP_USERDATA) as u32;
        if let Some(state) = state_from_hwnd(main) {
            state.dock_layout.set_floating_rect(id, rect);
        }
    }
    request_placement_check(main);
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
        // The front panel is a plugin's and its window has left: this
        // group is painting an empty band. A window that is destroyed,
        // or re-parented away, while it is on screen uncovers its
        // parent, so this paint is the first the host hears of it —
        // nothing tells a window that a child was re-parented away.
        // Ask for the sweep that closes the panel; a departed window
        // behind another tab waits for the next reconcile, with
        // nothing on screen to show for it meanwhile.
        let sweep_from = snapshot.as_ref().and_then(|(main, snap)| {
            let front = *snap.panels.get(snap.active)?;
            if !matches!(front, DockPanel::Plugin(_)) {
                return None;
            }
            let departed = state_from_hwnd(*main)?
                .dock_dialogs
                .iter()
                .any(|e| e.panel == front && !registration_stands(e.h_client));
            departed.then_some(*main)
        });
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(ghwnd, &raw mut ps);
        if let Some((_, snap)) = snapshot {
            let mut rc = RECT::default();
            let _ = GetClientRect(ghwnd, &raw mut rc);
            draw_group_chrome(hdc, ghwnd, rc.right, rc.bottom, &snap, icons, &plugin_icons);
        }
        let _ = EndPaint(ghwnd, &raw const ps);
        if let Some(main) = sweep_from {
            request_departure_sweep(main);
        }
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
                    // Only a panel still open is moved. One closed
                    // while the button was held — its plugin sent
                    // `NPPM_DMMHIDE`, or took its window away and the
                    // registration was retired — stays closed:
                    // `move_panel` reopens a closed panel at the
                    // target, and a retired one would come back as a
                    // group with no registration and nothing in it.
                    DragSubject::Panel(panel) => {
                        if drop_moves_panel(&state.dock_layout, panel) {
                            state.dock_layout.move_panel(panel, target);
                        }
                    }
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
                    let mut probe = state.dock_layout.clone();
                    probe.set_side_size(drag.side, proposed);
                    let frame = dock_area_frame(&probe, toolbar_height, rc.right, rc.bottom);
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
                    // The band has its new size. The checks the drag's
                    // layout passes asked for were held back while it
                    // went on, so the resize is reported once, here.
                    request_placement_check(main);
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

    /// What stops a change being told twice is an ordering no unit test
    /// can see: the record is *written* under the borrow, in
    /// `panel_notices`, and only then is anything sent, with none held,
    /// by `deliver_dock_notices`. A handler that shows or hides a panel
    /// reconciles again from inside the send; if the record were
    /// written after the send, that nested pass would find the change
    /// untold and send it again, from inside which the next pass would
    /// do the same.
    #[test]
    fn the_record_is_written_before_a_notification_is_sent() {
        let notify = own_body("notify_plugin_panels");
        let record = at(
            &notify,
            "panel_notices(&state.dock_layout, frame.as_ref(), &mut state.dock_dialogs)",
        )
        .expect("the reconcile no longer records what it owes under the borrow");
        let send = at(&notify, "deliver_dock_notices(main_hwnd, notices)")
            .expect("the reconcile no longer sends the notifications");
        assert!(record < send, "the send now precedes the record");

        let notices = own_body("panel_notices");
        assert!(
            at(
                &notices,
                "let owed = entry.told.update(layout, frame, entry.panel);"
            )
            .is_some(),
            "panel_notices no longer writes the record"
        );
        assert!(
            !notices.contains("SendMessageW") && !notices.contains("send_dock_notice"),
            "panel_notices sends while the caller's borrow is live"
        );
        // The record is the shared one's to write, in the one place:
        // nothing on the way to the send touches it again.
        for later in [
            "deliver_dock_notices",
            "drain_dock_notices",
            "queue_dock_notices",
            "send_dock_notice",
            "notice_target",
        ] {
            assert!(
                !own_body(later).contains(".told"),
                "`{later}` touches the record, after the send it must precede"
            );
        }
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
            told: codepp_plugin_host::docking::PanelTold::default(),
            serial: next_registration_serial(),
            caller: None,
        }
    }

    /// The carve of a 1000 × 700 main window with a 30-pixel toolbar,
    /// as the layout pass makes it.
    fn frame_for_test(layout: &DockLayout) -> codepp_core::dock::DockFrame {
        dock_area_frame(layout, 30, 1000, 700)
    }

    /// One reconcile's notices for `dialogs`, as `(serial, code)`, the
    /// main window laid out.
    fn notices_for_test(layout: &DockLayout, dialogs: &mut [crate::DockEntry]) -> Vec<(u64, u32)> {
        panel_notices(layout, Some(&frame_for_test(layout)), dialogs)
            .into_iter()
            .map(|n| (n.serial, n.code))
            .collect()
    }

    /// What a second `NPPM_DMMREGASDCKDLG` carries for `h_client`, with
    /// no tab icon offered.
    fn params_for_test(
        h_client: HWND,
        name: &str,
        module: &str,
        tb_data: *const codepp_plugin_host::TbData,
        caller: Option<usize>,
    ) -> codepp_plugin_host::DockDialogParams {
        codepp_plugin_host::DockDialogParams {
            h_client: h_client.0,
            name: name.into(),
            module_name: module.into(),
            add_info: None,
            h_icon_tab: core::ptr::null_mut(),
            rc_float: codepp_plugin_host::TbRect::default(),
            u_mask: codepp_plugin_host::DWS_DF_CONT_RIGHT,
            dlg_id: 7,
            i_prev_cont: 0,
            tb_data,
            caller,
        }
    }

    /// Registering a docked window again takes what the plugin says
    /// now — its `tTbData`, both names, the command and the mask — and
    /// keeps the panel and the window. The plugin is told its container
    /// again, except from inside the handler of such a notice. Under
    /// another module, or from a plugin the host can tell is another,
    /// it changes nothing.
    #[test]
    fn a_second_registration_refreshes_the_first() {
        let panel = codepp_shell::intern_plugin_dock_panel("Refresh A", "RefreshTest.dll")
            .expect("the identity");
        let h = HWND(0x5151 as *mut c_void);
        let first = 0x1000 as *const codepp_plugin_host::TbData;
        let second = 0x2000 as *const codepp_plugin_host::TbData;
        let mut entry = crate::DockEntry {
            tb_data: first,
            name: "Refresh A".into(),
            module_name: "RefreshTest.dll".into(),
            dlg_id: 3,
            caller: Some(2),
            ..entry_for_test(panel, h)
        };
        // Shown at the bottom, and told all a shown panel is owed: its
        // container, that it is on screen, and where it is laid out.
        let serial = entry.serial;
        let mut layout = DockLayout::new();
        layout.set_initial_side(panel, DockSide::Bottom);
        layout.show(panel);
        assert_eq!(
            notices_for_test(&layout, std::slice::from_mut(&mut entry)),
            vec![
                (serial, (3 << 16) | codepp_plugin_host::DMN_DOCK),
                (serial, codepp_plugin_host::DMN_SWITCHIN),
                (serial, codepp_plugin_host::DMN_FLOATDROPPED),
            ]
        );
        let told = entry.told;
        let untouched = |entry: &crate::DockEntry| {
            assert_eq!(entry.tb_data, first);
            assert_eq!(entry.name, "Refresh A");
            assert_eq!(entry.module_name, "RefreshTest.dll");
            assert_eq!((entry.dlg_id, entry.u_mask), (3, 0));
            assert_eq!(entry.told, told);
            assert_eq!(entry.serial, serial);
            assert_eq!(entry.caller, Some(2));
        };

        // Another module's registration of this window is refused,
        // whatever name it gives, and leaves the entry as it was.
        let refused = unsafe {
            crate::refresh_dock_registration(
                &mut entry,
                params_for_test(h, "Refresh A", "Other.dll", second, Some(2)),
                true,
            )
        };
        assert!(!refused);
        untouched(&entry);

        // So is one the host takes for another plugin than the first,
        // though it names the right module.
        let refused = unsafe {
            crate::refresh_dock_registration(
                &mut entry,
                params_for_test(h, "Refresh B", "RefreshTest.dll", second, Some(5)),
                true,
            )
        };
        assert!(!refused);
        untouched(&entry);

        // From inside a dock notice's handler the registration is
        // refreshed, and what the plugin was told stands.
        let taken = unsafe {
            crate::refresh_dock_registration(
                &mut entry,
                params_for_test(h, "Refresh A", "RefreshTest.dll", first, Some(2)),
                false,
            )
        };
        assert!(taken);
        assert_eq!(entry.told, told);

        // The same module's, in another case and under a new name, is
        // taken, from a sender the host cannot name: the plugin the
        // first registration was taken for stays on record.
        let taken = unsafe {
            crate::refresh_dock_registration(
                &mut entry,
                params_for_test(h, "Refresh B", "REFRESHTEST.DLL", second, None),
                true,
            )
        };
        assert!(taken);
        assert_eq!(entry.caller, Some(2));
        // Told again: the next reconcile finds no container on record
        // and sends the registration's notice — and only that one. The
        // panel is still on screen where it was, so it is neither
        // switched in nor laid out a second time. It is the same
        // registration, too, under the serial it had.
        assert_ne!(entry.told, told);
        assert_eq!(entry.serial, serial);
        assert_eq!(
            notices_for_test(&layout, std::slice::from_mut(&mut entry)),
            vec![(serial, (3 << 16) | codepp_plugin_host::DMN_DOCK)]
        );
        assert_eq!(entry.told, told);
        assert_eq!(entry.tb_data, second);
        assert_eq!(entry.name, "Refresh B");
        assert_eq!(entry.module_name, "REFRESHTEST.DLL");
        assert_eq!(entry.dlg_id, 7);
        assert_eq!(entry.u_mask, codepp_plugin_host::DWS_DF_CONT_RIGHT);
        // The panel is the identity it was registered under, so its
        // caption and its place in the session do not follow the name.
        assert_eq!(entry.panel, panel);
        assert_eq!(entry.panel.title(), "Refresh A");
        assert_eq!(entry.h_client, h);
        assert!(entry.tab_icon.is_none());

        // A registration the host could attribute to nobody stays so:
        // a plugin that refreshes it does not go on record for it, so
        // it cannot keep the window's own plugin from doing the same.
        let mut unattributed = crate::DockEntry {
            module_name: "RefreshTest.dll".into(),
            ..entry_for_test(panel, h)
        };
        for sender in [Some(5), Some(7)] {
            let taken = unsafe {
                crate::refresh_dock_registration(
                    &mut unattributed,
                    params_for_test(h, "Refresh A", "RefreshTest.dll", second, sender),
                    true,
                )
            };
            assert!(taken);
            assert_eq!(unattributed.caller, None);
        }
    }

    /// A refresh replaces the tab's icon, and deletes the one it had:
    /// the bitmap is the host's own copy, made at registration.
    #[test]
    fn a_refresh_lets_go_of_the_old_tab_icon() {
        use windows::Win32::Graphics::Gdi::{CreateBitmap, GetObjectW};

        let panel = codepp_shell::intern_plugin_dock_panel("Refresh Icon", "RefreshTest.dll")
            .expect("the identity");
        let h = HWND(0x5252 as *mut c_void);
        let old = unsafe { CreateBitmap(16, 16, 1, 32, None) };
        assert!(!old.is_invalid());
        let is_bitmap = |bmp: HBITMAP| {
            let mut info = BITMAP::default();
            unsafe {
                GetObjectW(
                    bmp.into(),
                    size_of::<BITMAP>() as i32,
                    Some((&raw mut info).cast()),
                ) != 0
            }
        };
        assert!(is_bitmap(old), "the test's own bitmap is not one");
        let mut entry = crate::DockEntry {
            tab_icon: Some(old),
            module_name: "RefreshTest.dll".into(),
            ..entry_for_test(panel, h)
        };
        // No icon offered now: the tab goes back to the host's glyph.
        let taken = unsafe {
            crate::refresh_dock_registration(
                &mut entry,
                params_for_test(
                    h,
                    "Refresh Icon",
                    "RefreshTest.dll",
                    core::ptr::null(),
                    None,
                ),
                true,
            )
        };
        assert!(taken);
        assert!(entry.tab_icon.is_none());
        assert!(!is_bitmap(old), "the old tab icon was leaked");
    }

    /// A notice about `Workspace` with `code`, for the queue tests: the
    /// drain looks at neither the panel nor the serial.
    fn notice_for_test(code: u32) -> DockNotice {
        DockNotice {
            panel: DockPanel::Workspace,
            serial: 0,
            code,
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

        const CHAIN: u32 = 12;
        thread_local! {
            static DEPTH: Cell<u32> = const { Cell::new(0) };
            static MAX_DEPTH: Cell<u32> = const { Cell::new(0) };
            static SEEN: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
        }
        /// What the drain's `send` does in production, to the probe.
        unsafe fn send_to(hwnd: HWND, notice: DockNotice) {
            let nmhdr = NMHDR {
                hwndFrom: HWND::default(),
                idFrom: 0,
                code: notice.code,
            };
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_NOTIFY,
                    Some(WPARAM(0)),
                    Some(LPARAM(&raw const nmhdr as isize)),
                );
            }
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
            // SAFETY: `send_to` passes a live NMHDR.
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
                drain_dock_notices(vec![notice_for_test(code + 1)], |notice| unsafe {
                    send_to(hwnd, notice);
                });
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
                lpszClassName: w!("DockTestNoticeProbe"),
                ..Default::default()
            };
            let _ = RegisterClassExW(&raw const class);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("DockTestNoticeProbe"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .expect("probe window");

            drain_dock_notices(vec![notice_for_test(1)], |notice| send_to(hwnd, notice));
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

    /// The queue bounds how deep a plugin's re-entry nests; the cap
    /// bounds how long it can go on. A handler that answers every
    /// notice with another — two plugins each bringing its own tab back
    /// to the front whenever told it went behind — is cut off after one
    /// delivery's worth, and the queue never holds more than that
    /// either, however many a handler offers it.
    #[test]
    fn a_delivery_is_cut_off_at_its_cap_and_the_queue_is_bounded() {
        use codepp_plugin_host::docking::MAX_NOTICES_PER_DELIVERY as CAP;
        use std::cell::Cell;

        // A burst larger than one delivery may send: the queue refuses
        // what is past the cap, and the delivery sends the rest.
        let sent = Cell::new(0usize);
        drain_dock_notices(vec![notice_for_test(1); CAP + 50], |_| {
            sent.set(sent.get() + 1);
        });
        assert_eq!(sent.get(), CAP);
        assert!(DOCK_NOTICES.with(|q| q.borrow().is_empty()));

        // A handler that answers every notice with another would go on
        // for ever; the delivery stops after its cap and leaves nothing
        // queued behind it.
        let sent = Cell::new(0usize);
        drain_dock_notices(vec![notice_for_test(1)], |notice| {
            sent.set(sent.get() + 1);
            drain_dock_notices(vec![notice], |_| panic!("a nested delivery sent"));
        });
        assert_eq!(
            sent.get(),
            CAP,
            "one delivery works through exactly its cap"
        );
        assert!(
            DOCK_NOTICES.with(|q| q.borrow().is_empty()),
            "what was still queued at the cap was left for the next delivery"
        );
        assert_eq!(
            DOCK_NOTICES_DROPPED.with(Cell::get),
            0,
            "what was dropped was not reported and reset"
        );

        // From inside a delivery, a handler offers half as many again
        // as one delivery may send: the queue takes no more than the
        // cap.
        let held = Cell::new(0usize);
        let refused = Cell::new(0usize);
        drain_dock_notices(vec![notice_for_test(1)], |_| {
            if held.get() == 0 {
                drain_dock_notices(vec![notice_for_test(2); CAP + CAP / 2], |_| {
                    panic!("a nested delivery sent")
                });
                held.set(DOCK_NOTICES.with(|q| q.borrow().len()));
                refused.set(DOCK_NOTICES_DROPPED.with(Cell::get));
            }
        });
        assert_eq!(held.get(), CAP, "the queue grew past one delivery's cap");
        assert_eq!(
            refused.get(),
            CAP / 2,
            "what the queue refused is not counted for the delivery's warning"
        );
        assert_eq!(DOCK_NOTICES_DROPPED.with(Cell::get), 0);

        // And a later delivery is not held to the last one's count.
        let sent = Cell::new(0usize);
        drain_dock_notices(vec![notice_for_test(1); 3], |_| sent.set(sent.get() + 1));
        assert_eq!(sent.get(), 3);
    }

    /// Puts the thread's quit flag back when a test that raised it
    /// ends, however it ends.
    struct QuitForTest;

    impl QuitForTest {
        fn begin() -> Self {
            crate::QUITTING.with(|q| q.set(true));
            Self
        }
    }

    impl Drop for QuitForTest {
        fn drop(&mut self) {
            crate::QUITTING.with(|q| q.set(false));
        }
    }

    /// Once the quit has begun the plugins hear nothing more of the
    /// dock: a delivery under way stops and drops what it had queued,
    /// and nothing is queued or sent after that.
    #[test]
    fn a_delivery_stops_and_drops_its_queue_once_the_quit_has_begun() {
        use std::cell::{Cell, RefCell};

        let sent = Cell::new(0usize);
        let quit = RefCell::new(None);
        // The first notice's handler begins the quit, as a plugin does
        // by closing the main window.
        drain_dock_notices(vec![notice_for_test(1); 3], |_| {
            sent.set(sent.get() + 1);
            *quit.borrow_mut() = Some(QuitForTest::begin());
        });
        assert_eq!(sent.get(), 1, "a delivery went on after the quit had begun");
        assert!(
            DOCK_NOTICES.with(|q| q.borrow().is_empty()),
            "notices outlived the quit in the queue"
        );
        drain_dock_notices(vec![notice_for_test(2)], |_| {
            panic!("a notice was sent after the quit had begun")
        });
        assert!(
            DOCK_NOTICES.with(|q| q.borrow().is_empty()),
            "a notice was queued after the quit had begun"
        );
        drop(quit);
    }

    /// A delivery is known to be running exactly while one is: what a
    /// registration refreshed from a notice's handler goes by, so as
    /// not to be answered with another notice.
    #[test]
    fn a_delivery_is_known_to_be_running_only_while_it_is() {
        assert!(!delivering_dock_notices());
        let delivering = NoticeDelivery::enter().expect("no delivery is running yet");
        assert!(delivering_dock_notices());
        drop(delivering);
        assert!(!delivering_dock_notices());
        // And from inside a handler, which is where it is asked.
        let asked = std::cell::Cell::new(false);
        drain_dock_notices(vec![notice_for_test(1)], |_| {
            asked.set(delivering_dock_notices());
        });
        assert!(asked.get());
        assert!(!delivering_dock_notices());
    }

    /// An unwind out of a drain must leave nothing behind: not the
    /// flag (every later notice would queue behind a drain that is
    /// gone) and not the queue (a notice left there would go out with
    /// some later delivery, long after it was true).
    #[test]
    fn an_unwinding_drain_clears_the_flag_and_discards_the_queue() {
        let caught = std::panic::catch_unwind(|| {
            drain_dock_notices(vec![notice_for_test(1); 3], |_| {
                panic!("simulated failure mid-drain")
            });
        });
        assert!(caught.is_err(), "precondition: the closure unwound");
        assert!(
            !DOCK_NOTICES_DELIVERING.with(std::cell::Cell::get),
            "the drain flag survived the unwind"
        );
        assert!(
            DOCK_NOTICES.with(|q| q.borrow().is_empty()),
            "queued notices survived the unwind"
        );
    }

    /// The plugin's handler for a `DMN_*` runs inside the send, and may
    /// send `NPPM_*` straight back. That is answered only if no
    /// `WindowState` borrow is live across the send — the discipline
    /// the three sibling guards in `lib.rs` pin for notification
    /// delivery. The failure mode is the quiet one: move the send
    /// *into* the borrowing closure and it still compiles, reads
    /// naturally, and turns every `NPPM_*` from the handler into a
    /// decline — or into aliasing.
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

        // The reconcile's last phase: the notices are taken under a
        // borrow that ends before they are handed to the delivery.
        let notify = code_only(&fn_body(dock_src, "notify_plugin_panels"));
        let start = notify
            .find("let notices = unsafe { state_from_hwnd(main_hwnd) }")
            .expect("the notices are no longer taken under a borrow");
        let send = notify
            .find("deliver_dock_notices(main_hwnd")
            .expect("the notices are no longer delivered");
        assert!(
            send > statement_end(&notify, start),
            "the notices are delivered from inside the borrow that computed them"
        );

        // The queue itself never touches the state.
        for queue in ["deliver_dock_notices", "drain_dock_notices"] {
            let body = code_only(&fn_body(dock_src, queue));
            assert!(
                !body.contains("state_from_hwnd") && !body.contains("PluginCallGuard"),
                "`{queue}` takes a state borrow or arms the plugin guard"
            );
        }

        // The sender resolves its target under a borrow that ends
        // before the send, and takes no other.
        let sender = code_only(&fn_body(dock_src, "send_dock_notice"));
        let start = sender
            .find("let target = unsafe { state_from_hwnd(main_hwnd) }")
            .expect("send_dock_notice no longer resolves its target under a borrow");
        let end = statement_end(&sender, start);
        let send = sender
            .find("SendMessageW(")
            .expect("send_dock_notice no longer sends");
        assert!(send > end, "a dock notice is sent from inside the borrow");
        assert_eq!(
            sender.matches("state_from_hwnd").count(),
            1,
            "a second state borrow is taken on the way to the send"
        );
        assert!(
            !sender.contains("PluginCallGuard"),
            "a dock notice's handler would have every NPPM_* declined"
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

    /// The whole send policy, through the real `DockEntry` records: the
    /// shared rule decides what each change owes and in what order, and
    /// this backend passes it the carve its layout pass uses.
    #[test]
    fn panel_notices_tell_each_plugin_what_a_change_means_for_its_panel() {
        use codepp_plugin_host::{
            DMN_DOCK, DMN_FLOAT, DMN_FLOATDROPPED, DMN_SWITCHIN, DMN_SWITCHOFF,
        };

        let panel =
            codepp_core::dock::intern_plugin_panel("cn.dll", "Notice Panel").expect("intern");
        let other =
            codepp_core::dock::intern_plugin_panel("cn2.dll", "Other Panel").expect("intern");
        let mut dialogs = vec![entry_for_test(panel, HWND(0x1234 as *mut c_void))];
        let first = dialogs[0].serial;
        let mut l = DockLayout::new();
        l.set_initial_side(panel, DockSide::Bottom);
        l.set_initial_side(other, DockSide::Bottom);
        let bottom = (3 << 16) | DMN_DOCK;

        // Registration, not yet shown: told where it will open.
        assert_eq!(notices_for_test(&l, &mut dialogs), vec![(first, bottom)]);
        // Nothing changed: nothing said.
        assert!(notices_for_test(&l, &mut dialogs).is_empty());

        // Shown where it was predicted to open: on screen, laid out.
        l.show(panel);
        assert_eq!(
            notices_for_test(&l, &mut dialogs),
            vec![(first, DMN_SWITCHIN), (first, DMN_FLOATDROPPED)]
        );

        // A second panel registers and is shown into the same
        // container, in front. Its container first, then the panel
        // coming in, then the one going behind it, then both laid out
        // anew, since the second tab brings the tab bar.
        dialogs.push(entry_for_test(other, HWND(0x1235 as *mut c_void)));
        let second = dialogs[1].serial;
        assert_ne!(first, second, "two registrations took one serial");
        l.show(other);
        assert_eq!(
            notices_for_test(&l, &mut dialogs),
            vec![
                (second, bottom),
                (second, DMN_SWITCHIN),
                (first, DMN_SWITCHOFF),
                (first, DMN_FLOATDROPPED),
                (second, DMN_FLOATDROPPED),
            ]
        );

        // A tab switch tells the one coming in before the one it
        // replaced, and lays nothing out anew.
        l.activate(panel);
        assert_eq!(
            notices_for_test(&l, &mut dialogs),
            vec![(first, DMN_SWITCHIN), (second, DMN_SWITCHOFF)]
        );
        // The tab already in front, clicked again: nothing.
        l.activate(panel);
        assert!(notices_for_test(&l, &mut dialogs).is_empty());

        // The window resized with nothing else changed: the bottom
        // band is laid out anew, and both of its panels hear of it,
        // the one behind the front tab included.
        let resized = dock_area_frame(&l, 30, 900, 700);
        assert_eq!(
            panel_notices(&l, Some(&resized), &mut dialogs)
                .into_iter()
                .map(|n| (n.serial, n.code))
                .collect::<Vec<_>>(),
            vec![(first, DMN_FLOATDROPPED), (second, DMN_FLOATDROPPED)]
        );
        // And back, so the steps below start from the fixture's size.
        assert_eq!(notices_for_test(&l, &mut dialogs).len(), 2);

        // Floated: a new container, laid out anew, still in front. The
        // tab it left behind comes on screen where it now is.
        l.move_panel(panel, DropTarget::Floating(DockRect::new(0, 0, 300, 200)));
        let sent = notices_for_test(&l, &mut dialogs);
        assert_eq!(sent[0].0, first);
        assert_eq!(sent[0].1 & 0xFFFF, DMN_FLOAT);
        assert_eq!(
            sent[1..],
            [
                (second, DMN_SWITCHIN),
                (first, DMN_FLOATDROPPED),
                (second, DMN_FLOATDROPPED)
            ]
        );

        // Hidden: told nothing. Shown again while floating: on screen
        // and laid out, in the container it was already told of.
        l.hide(panel);
        assert!(notices_for_test(&l, &mut dialogs).is_empty());
        l.show(panel);
        assert_eq!(
            notices_for_test(&l, &mut dialogs),
            vec![(first, DMN_SWITCHIN), (first, DMN_FLOATDROPPED)]
        );

        // Docked on the left: DMN_DOCK with CONT_LEFT, the bare code.
        l.move_panel(panel, DropTarget::Side(DockSide::Left));
        let sent = notices_for_test(&l, &mut dialogs);
        assert_eq!(sent[0], (first, DMN_DOCK));
        assert!(sent.contains(&(first, DMN_FLOATDROPPED)));
        assert!(
            !sent.contains(&(first, DMN_SWITCHIN)),
            "a panel that stayed in front through a move was switched in again"
        );

        // While the main window has no size, a docked panel has no
        // place to be told of; it hears of one once the window has.
        let mut fresh = vec![entry_for_test(panel, HWND(0x1236 as *mut c_void))];
        let third = fresh[0].serial;
        assert_eq!(
            panel_notices(&l, None, &mut fresh)
                .into_iter()
                .map(|n| (n.serial, n.code))
                .collect::<Vec<_>>(),
            vec![(third, DMN_DOCK), (third, DMN_SWITCHIN)]
        );
        assert_eq!(
            notices_for_test(&l, &mut fresh),
            vec![(third, DMN_FLOATDROPPED)]
        );
    }

    /// The carve the notices read is the one the layout pass positions
    /// windows by: a docked group's rectangle in it is where
    /// `layout_children` puts the group's window.
    #[test]
    fn the_dock_area_is_carved_between_the_toolbar_and_the_status_bar() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let group = l.group_of(DockPanel::Workspace).expect("shown").id;
        let frame = dock_area_frame(&l, 30, 1000, 700);
        let rect = frame.group_rect(group).expect("a docked group has a rect");
        assert_eq!(
            (rect.x, rect.y),
            (0, 30),
            "the band starts under the toolbar"
        );
        assert_eq!(
            rect.h,
            700 - 30 - STATUS_HEIGHT_PX,
            "and ends at the status bar"
        );
        // The editor keeps what the band leaves, to the right edge.
        assert_eq!(frame.editor.x + frame.editor.w, 1000);
        // A taller toolbar moves the band down and shortens it.
        let taller = dock_area_frame(&l, 50, 1000, 700);
        let rect = taller.group_rect(group).expect("a docked group has a rect");
        assert_eq!((rect.y, rect.h), (50, 700 - 50 - STATUS_HEIGHT_PX));
    }

    /// A queued notice goes to the registration it was raised for, from
    /// the window Notepad++ sends it from, if it is still worth
    /// sending when its turn comes.
    #[test]
    fn a_queued_notice_finds_its_registration_and_its_sender() {
        use codepp_plugin_host::{DMN_DOCK, DMN_FLOATDROPPED, DMN_SWITCHIN, DMN_SWITCHOFF};

        let front = codepp_core::dock::intern_plugin_panel("nt.dll", "Front").expect("intern");
        let behind = codepp_core::dock::intern_plugin_panel("nt.dll", "Behind").expect("intern");
        let closed = codepp_core::dock::intern_plugin_panel("nt.dll", "Closed").expect("intern");
        let (h_front, h_behind, h_closed) = (
            HWND(0x7001 as *mut c_void),
            HWND(0x7002 as *mut c_void),
            HWND(0x7003 as *mut c_void),
        );
        let main = HWND(0x7100 as *mut c_void);
        let group_hwnd = HWND(0x7200 as *mut c_void);
        let dialogs = vec![
            entry_for_test(front, h_front),
            entry_for_test(behind, h_behind),
            entry_for_test(closed, h_closed),
        ];
        let mut l = DockLayout::new();
        for panel in [front, behind, closed] {
            l.set_initial_side(panel, DockSide::Bottom);
        }
        l.show(behind);
        l.show(front);
        assert!(l.is_active(front) && l.is_visible(behind) && !l.is_active(behind));
        let groups = vec![DockGroupWindow {
            id: l.group_of(front).expect("shown").id,
            hwnd: group_hwnd,
            floating: false,
        }];
        let all = |_: HWND| true;
        let notice = |entry: &crate::DockEntry, code: u32| DockNotice {
            panel: entry.panel,
            serial: entry.serial,
            code,
        };
        let target = |n: &DockNotice| notice_target(&dialogs, &l, &groups, main, n, all);

        // The container notices come from the main window, the only
        // sender Notepad++'s docking-dialog template accepts — with a
        // container number in the high word or without.
        assert_eq!(
            target(&notice(&dialogs[0], (3 << 16) | DMN_DOCK)),
            Some((h_front, main))
        );
        assert_eq!(
            target(&notice(&dialogs[2], DMN_DOCK)),
            Some((h_closed, main)),
            "a hidden panel is still told its container"
        );
        // The other three come from the panel's container.
        for code in [DMN_SWITCHIN, DMN_FLOATDROPPED] {
            assert_eq!(
                target(&notice(&dialogs[0], code)),
                Some((h_front, group_hwnd))
            );
        }
        assert_eq!(
            target(&notice(&dialogs[1], DMN_SWITCHOFF)),
            Some((h_behind, group_hwnd))
        );
        assert_eq!(
            target(&notice(&dialogs[1], DMN_FLOATDROPPED)),
            Some((h_behind, group_hwnd)),
            "a panel behind the front tab is laid out too"
        );

        // No longer true by its turn: dropped.
        assert_eq!(
            target(&notice(&dialogs[1], DMN_SWITCHIN)),
            None,
            "a switch-in reached a panel that is no longer in front"
        );
        assert_eq!(
            target(&notice(&dialogs[2], DMN_FLOATDROPPED)),
            None,
            "a relayout reached a panel that is in no group"
        );
        assert_eq!(
            target(&notice(&dialogs[2], DMN_SWITCHOFF)),
            None,
            "a switch-off with no container to come from was sent"
        );

        // The registration must be the one the notice was raised for,
        // and still stand. One made since for the same panel at the
        // same handle value — the window destroyed, a new one given
        // its handle and registered — is another registration.
        let stale = DockNotice {
            serial: dialogs[0].serial + 1000,
            ..notice(&dialogs[0], DMN_DOCK)
        };
        assert_eq!(target(&stale), None);
        assert_eq!(
            notice_target(
                &dialogs,
                &l,
                &groups,
                main,
                &notice(&dialogs[0], DMN_DOCK),
                |h| h != h_front
            ),
            None,
            "a notice reached a window that has left"
        );

        // A group with no window — not made yet, or failed to be — has
        // nothing to send from.
        assert_eq!(
            notice_target(
                &dialogs,
                &l,
                &[],
                main,
                &notice(&dialogs[0], DMN_SWITCHIN),
                all
            ),
            None
        );
        let null_group = [DockGroupWindow {
            hwnd: HWND::default(),
            ..groups[0]
        }];
        assert_eq!(
            notice_target(
                &dialogs,
                &l,
                &null_group,
                main,
                &notice(&dialogs[0], DMN_SWITCHIN),
                all
            ),
            None
        );
    }

    /// Every registration takes a serial of its own.
    #[test]
    fn registrations_take_serials_that_never_repeat() {
        let first = next_registration_serial();
        let second = next_registration_serial();
        let third = next_registration_serial();
        assert!(first < second && second < third);
    }

    /// A hidden window of this thread's, for a timer to be set on. It
    /// is never shown and has no `WindowState` behind it.
    unsafe fn window_for_timer_test() -> HWND {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(GetModuleHandleW(None).unwrap_or_default().into()),
                None,
            )
            .expect("a window for the timer")
        }
    }

    /// Whether the placement timer is set on `hwnd`, found out by
    /// killing it: `KillTimer` answers whether there was one.
    fn take_placement_timer(hwnd: HWND) -> bool {
        // SAFETY: `KillTimer` takes any handle value.
        unsafe { KillTimer(Some(hwnd), PLACEMENT_TIMER_ID) }.is_ok()
    }

    /// A resize lays the window out many times and asks for one check:
    /// however often it is asked for, there is one timer. And the check
    /// that answers it is one shot: it stops the timer before anything
    /// else, whatever it then finds — here no state, so nothing to
    /// tell — and the next layout pass sets it again.
    #[test]
    fn a_placement_check_is_asked_for_once_until_it_has_run() {
        let hwnd = unsafe { window_for_timer_test() };
        assert!(!take_placement_timer(hwnd), "precondition: no timer yet");

        arm_placement_check(hwnd);
        arm_placement_check(hwnd);
        arm_placement_check(hwnd);
        assert!(take_placement_timer(hwnd), "asking set no timer");
        assert!(
            !take_placement_timer(hwnd),
            "a timer is set for every layout pass"
        );

        // A timer something else stopped — a plugin is handed this
        // window, and the timer just taken away stands for one that
        // called `KillTimer` on it — does not keep the next request
        // from setting it: nothing of the host's remembers the first.
        arm_placement_check(hwnd);
        assert!(
            take_placement_timer(hwnd),
            "a timer that was stopped is never set again"
        );

        // The check: its timer gone, and the next request sets it again.
        arm_placement_check(hwnd);
        unsafe { check_plugin_panel_placements(hwnd) };
        assert!(
            !take_placement_timer(hwnd),
            "the check left its timer ticking"
        );
        arm_placement_check(hwnd);
        assert!(take_placement_timer(hwnd));

        // A check that finds the quit begun still stops its timer:
        // nothing must keep ticking into the teardown.
        arm_placement_check(hwnd);
        let quit = QuitForTest::begin();
        unsafe { check_plugin_panel_placements(hwnd) };
        drop(quit);
        assert!(!take_placement_timer(hwnd));

        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        // A timer that cannot be set — the window is gone — is no
        // failure of the request's.
        arm_placement_check(hwnd);
    }

    /// With no plugin panel registered — here a window with no state at
    /// all — a layout pass sets no timer.
    ///
    /// That is all this can show: the two other reasons to ask for
    /// nothing, a reconcile under way and a quit begun, return before
    /// the state is read, and with no state nothing is asked for either
    /// way. Those two are pinned by the source guard below
    /// (`geometry_changes_ask_for_the_placement_check_and_never_notify`).
    #[test]
    fn no_placement_check_is_asked_for_without_plugin_panels() {
        let hwnd = unsafe { window_for_timer_test() };
        request_placement_check(hwnd);
        assert!(
            !take_placement_timer(hwnd),
            "a check was asked for with no plugin panel registered"
        );
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    /// The geometry changes that go through no reconcile ask for the
    /// placement check and tell nobody themselves: they run inside
    /// `WM_SIZE`, which Windows sends from wherever a window was
    /// resized. And the check holds back while the user is still
    /// dragging, whose end asks again, so a resize is reported once.
    #[test]
    fn geometry_changes_ask_for_the_placement_check_and_never_notify() {
        use crate::plugin_reentry_guards::{code_only, production_src};

        // A reconcile tells too, as its last phase.
        let tells = [
            "notify_plugin_panels(",
            "deliver_dock_notices(",
            "send_dock_notice(",
            "apply_dock_layout(",
        ];
        let relayout = lib_body("relayout_now");
        assert!(
            relayout.ends_with("dock_panels::request_placement_check(main_hwnd);}"),
            "a layout pass no longer asks for the placement check"
        );
        // The toolbar height the layout pass is given is the one the
        // notices' carve is given: two expressions for it would be two
        // carves.
        assert!(
            at(
                &relayout,
                "layout_children( toolbar_hwnd, toolbar::toolbar_height_px(toolbar_bitmap_px),"
            )
            .is_some(),
            "the layout pass is given another toolbar height than the notices' carve"
        );
        let group_size = own_body("on_group_size");
        assert!(
            at(
                &group_size,
                "if snap.floating { mirror_floating_rect(main, ghwnd); }"
            )
            .is_some(),
            "a floating group's resize is no longer mirrored into the model"
        );
        // A float's resize and its move both go through the mirror,
        // which writes the model and then asks.
        let mirror = own_body("mirror_floating_rect");
        let write = at(&mirror, "state.dock_layout.set_floating_rect(id, rect);")
            .expect("a floating group's rectangle is no longer mirrored into the model");
        assert!(
            mirror.ends_with("request_placement_check(main);}")
                && write < mirror.len() - "request_placement_check(main);}".len(),
            "a floating group's resize or move no longer asks for the placement check"
        );
        assert!(
            at(
                &own_body("dock_group_wnd_proc"),
                "WM_MOVE => { if let Some((main, snap)) = group_snapshot(hwnd) { \
                 if snap.floating { mirror_floating_rect(main, hwnd); } } LRESULT(0) }"
            )
            .is_some(),
            "a floating group moved without being resized is no longer mirrored or reported"
        );
        for (name, body) in [
            ("relayout_now", &relayout),
            ("on_group_size", &group_size),
            ("mirror_floating_rect", &mirror),
        ] {
            for tell in tells {
                assert!(
                    !body.contains(tell),
                    "`{name}` tells plugins from inside a layout pass: `{tell}`"
                );
            }
        }
        let request = own_body("request_placement_check");
        assert!(
            request.starts_with(&squashed(
                "{ if crate::quitting() || RECONCILING.with(std::cell::Cell::get) { return; }"
            )),
            "a check is asked for during a quit, or from inside a reconcile"
        );
        assert!(
            at(
                &request,
                "is_some_and(|state| !state.dock_dialogs.is_empty());"
            )
            .is_some()
                && request.ends_with("ifplugin_panels{arm_placement_check(main_hwnd);}}"),
            "a check is asked for with no plugin panel registered"
        );
        // Asked for with a timer, the last thing a message loop is
        // given, and never with a posted message, which is handed out
        // before input: a plugin that answers every notice with a
        // resize would otherwise keep the keyboard and the mouse
        // waiting.
        let arm = own_body("arm_placement_check");
        assert_eq!(
            arm,
            squashed(
                "{ let _ = unsafe { SetTimer(Some(main_hwnd), PLACEMENT_TIMER_ID, 0, None) }; }"
            ),
            "the placement check is no longer asked for with its one timer, and nothing else"
        );
        for tell in tells {
            assert!(!request.contains(tell) && !arm.contains(tell));
        }

        // The check: timer stopped first, then held back, then told.
        assert_eq!(
            own_body("check_plugin_panel_placements"),
            squashed(
                "{ let _ = unsafe { KillTimer(Some(main_hwnd), PLACEMENT_TIMER_ID) }; \
                 if crate::quitting() || unsafe { resize_under_way(main_hwnd) } { return; } \
                 unsafe { notify_plugin_panels(main_hwnd) }; }"
            ),
            "the placement check no longer stops its timer first, holds back during a \
             resize, and then tells the plugins"
        );
        // Held back for any one of the three, each asked of Windows or
        // of the state as it is now.
        let under_way = own_body("resize_under_way");
        assert!(
            at(
                &under_way,
                "let in_move_size = unsafe { GetGUIThreadInfo(GetCurrentThreadId(), &raw mut info) } \
                 .is_ok() && info.flags.contains(GUI_INMOVESIZE);"
            )
            .is_some(),
            "the check no longer asks Windows whether a frame is being dragged"
        );
        assert!(
            under_way.ends_with(&squashed(
                "in_move_size || unsafe { IsIconic(main_hwnd) }.as_bool() \
                 || unsafe { state_from_hwnd(main_hwnd) } \
                 .is_some_and(|state| state.dock_side_drag.is_some()) }"
            )),
            "the check is no longer held back by a frame drag, a minimized window or a \
             splitter drag, each on its own"
        );

        // Each of those ends by asking again.
        let lib = squashed(&code_only(production_src()));
        assert!(
            at(
                &lib,
                "if wparam.0 == dock_panels::PLACEMENT_TIMER_ID { \
                 dock_panels::check_plugin_panel_placements(hwnd); return LRESULT(0); }"
            )
            .is_some(),
            "nothing answers the placement check's timer"
        );
        assert!(
            at(
                &lib,
                "let _ = KillTimer(Some(hwnd), dock_panels::PLACEMENT_TIMER_ID);"
            )
            .is_some(),
            "the placement timer is left ticking into the main window's teardown"
        );
        assert!(
            !lib.contains("WM_APP_DOCK")
                || lib.matches("WM_APP_DOCK").count() == lib.matches("WM_APP_DOCK_SWEEP").count(),
            "the placement check is posted as a message again"
        );
        assert!(
            at(
                &lib,
                "WM_EXITSIZEMOVE => { dock_panels::request_placement_check(hwnd); \
                 DefWindowProcW(hwnd, msg, wparam, lparam) }"
            )
            .is_some(),
            "the end of a drag of the main window's frame no longer asks"
        );
        let group = own_body("dock_group_wnd_proc");
        assert!(
            at(
                &group,
                "WM_EXITSIZEMOVE => { if let Some(main) = find_main_hwnd(hwnd) { \
                 request_placement_check(main); } DefWindowProcW(hwnd, msg, wparam, lparam) }"
            )
            .is_some(),
            "the end of a drag of a floating group's frame no longer asks"
        );
        let splitter = own_body("dock_side_splitter_wnd_proc");
        let released = at(&splitter, "state.dock_side_drag = None;").expect("the release");
        let asks = at(&splitter, "request_placement_check(main);")
            .expect("the end of a splitter drag no longer asks");
        assert!(
            released < asks,
            "the splitter asks while its drag still reads as under way"
        );

        // The plugins are told from two places: a reconcile's last
        // phase, and the check.
        let dock = include_str!("dock_panels.rs");
        let dock = &dock[..dock.find("#[cfg(test)]").expect("test module")];
        let dock = squashed(&code_only(dock));
        assert_eq!(
            dock.matches("notify_plugin_panels(").count(),
            3,
            "plugins are told from somewhere other than the reconcile and the placement check"
        );
        assert!(!lib.contains("notify_plugin_panels("));

        // And the notices read the carve the layout pass lays out by.
        assert_eq!(
            dock.matches("compute_frame(").count() + lib.matches("compute_frame(").count(),
            1,
            "the dock area is carved in more than one place"
        );
        assert!(
            at(
                &lib_body("layout_children"),
                "let frame = dock_panels::dock_area_frame(&dock_chrome.layout, toolbar_height, \
                 width, height);"
            )
            .is_some(),
            "the layout pass carves the dock area for itself"
        );
        let now = own_body("dock_frame_now");
        assert!(
            at(
                &now,
                "(rc.right > 0 && rc.bottom > 0).then(|| { dock_area_frame( &state.dock_layout, \
                 toolbar::toolbar_height_px(state.toolbar_bitmap_px), rc.right, rc.bottom, ) })"
            )
            .is_some(),
            "the notices' carve is no longer the layout pass's: the layout, the toolbar's \
             height and the client area it is given, and none for a window with no client \
             area — minimized, or not laid out yet"
        );
        assert!(
            at(
                &own_body("notify_plugin_panels"),
                "let frame = dock_frame_now(main_hwnd, state);"
            )
            .is_some(),
            "the notices no longer read the main window's carve"
        );

        // A group sizes every tab's content, not only the front one's:
        // a panel behind the front tab is told it was laid out anew
        // too, and must find its window at the new size.
        assert!(
            at(
                &group_size,
                "let contents: Vec<HWND> = snap.panels.iter() \
                 .map(|p| panel_content_hwnd(state, *p)).collect();"
            )
            .is_some()
                && at(
                    &group_size,
                    "for content in contents { if !content.0.is_null() \
                     && GetParent(content).unwrap_or_default() == ghwnd {"
                )
                .is_some(),
            "a group no longer sizes the content of every panel that is its child"
        );
    }

    /// What the queue's rules rest on, which no unit test can see: each
    /// notice is checked when its turn comes and sent from the right
    /// window; every one goes out through the one sender, behind its
    /// own quit check; and a registration keeps the serial it took.
    #[test]
    fn dock_notices_go_out_through_one_checked_sender() {
        use crate::plugin_reentry_guards::{code_only, production_src};

        let drain = own_body("drain_dock_notices");
        assert!(
            drain.starts_with(&squashed(
                "{ if crate::quitting() { return; } queue_dock_notices(notices); \
                 let Some(_delivering) = NoticeDelivery::enter() else { return; }; \
                 let mut taken = 0usize; \
                 while let Some(notice) = DOCK_NOTICES.with(|q| q.borrow_mut().pop_front()) { \
                 taken += 1; \
                 if taken > codepp_plugin_host::docking::MAX_NOTICES_PER_DELIVERY {"
            )),
            "the delivery no longer queues, latches, counts and caps, in that order"
        );
        let quit = at(
            &drain,
            "if crate::quitting() { DOCK_NOTICES.with(|q| q.borrow_mut().clear()); break; }",
        )
        .expect("a delivery under way goes on after the quit has begun");
        let send = at(&drain, "send(notice);").expect("the delivery no longer sends");
        assert!(quit < send);
        assert_eq!(drain.matches("send(").count(), 1);
        assert!(
            at(
                &own_body("queue_dock_notices"),
                "let room = codepp_plugin_host::docking::MAX_NOTICES_PER_DELIVERY\
                 .saturating_sub(q.len()); let refused = notices.len().saturating_sub(room); \
                 q.extend(notices.into_iter().take(room));"
            )
            .is_some(),
            "the queue is no longer bounded by one delivery's cap"
        );

        // The sender: its own quit check first, then the target, then
        // the send, from the window the target names.
        let sender = own_body("send_dock_notice");
        assert!(
            sender.starts_with(&squashed("{ if crate::quitting() { return; }")),
            "the sender itself no longer stops at the quit"
        );
        let target = at(&sender, "notice_target(").expect("a notice is sent unchecked");
        let stands =
            at(&sender, "registration_stands,").expect("a notice goes to a window that has left");
        let bail = at(
            &sender,
            "let Some((h_client, from)) = target else { return; };",
        )
        .expect("a notice with no target is sent anyway");
        let send = at(&sender, "SendMessageW(").expect("the send");
        assert!(target < stands && stands < bail && bail < send);
        assert!(
            at(
                &sender,
                "let nmhdr = NMHDR { hwndFrom: from, idFrom: 0, code: notice.code, };"
            )
            .is_some(),
            "a dock notice no longer has its target's sender, idFrom 0 and its own code"
        );
        assert!(
            at(
                &sender,
                "SendMessageW( h_client, WM_NOTIFY, Some(WPARAM(0)),"
            )
            .is_some(),
            "a dock notice must travel as WM_NOTIFY to the plugin's h_client, wParam 0"
        );
        let resolve = own_body("notice_target");
        for rule in [
            ".find(|e| e.serial == notice.serial && stands(e.h_client))?;",
            "if !codepp_plugin_host::docking::still_applies(layout, entry.panel, notice.code) { \
             return None; }",
            "codepp_plugin_host::DMN_DOCK | codepp_plugin_host::DMN_FLOAT => main_hwnd,",
            ".find(|gw| gw.id == group && !gw.hwnd.is_invalid())? .hwnd",
        ] {
            assert!(
                at(&resolve, rule).is_some(),
                "a queued notice is no longer checked by `{rule}`"
            );
        }

        // One sender, one way to it, one `SendMessageW` in the module.
        let dock = include_str!("dock_panels.rs");
        let dock = &dock[..dock.find("#[cfg(test)]").expect("test module")];
        let dock = squashed(&code_only(dock));
        assert_eq!(
            dock.matches("send_dock_notice(").count(),
            2,
            "a dock notice is sent from somewhere other than the delivery"
        );
        assert_eq!(dock.matches("drain_dock_notices(").count(), 2);
        assert_eq!(dock.matches("deliver_dock_notices(").count(), 2);
        assert_eq!(
            dock.matches("SendMessageW(").count(),
            1,
            "something in the dock module sends a message past the sender's checks"
        );

        // A registration takes the next serial, and nothing changes it
        // afterwards — a refresh least of all.
        let lib = squashed(&code_only(production_src()));
        assert_eq!(lib.matches("next_registration_serial(").count(), 1);
        assert!(
            at(
                &lib_body("register_dock_dialog"),
                "serial: dock_panels::next_registration_serial(),"
            )
            .is_some(),
            "a registration no longer takes the next serial"
        );
        assert_eq!(
            lib.matches(".serial").count(),
            0,
            "something outside the dock module reads or writes a registration's serial"
        );
        assert_eq!(
            dock.matches(".serial=").count(),
            dock.matches(".serial==").count(),
            "the dock module writes a registration's serial, which only a registration sets"
        );
        assert!(
            at(
                &lib_body("refresh_dock_registration"),
                "if tell_again { entry.told.forget_container(); }"
            )
            .is_some(),
            "a refreshed registration is no longer told its container again, and only that"
        );
        assert!(
            at(
                &own_body("next_registration_serial"),
                "NEXT_REGISTRATION.with(|next| next.replace(next.get() + 1))"
            )
            .is_some()
                && dock.matches("NEXT_REGISTRATION").count() == 2,
            "something other than a registration touches the serial counter"
        );
    }

    /// A hidden window of the dock-group class, with no main window
    /// above it, so its procedure finds no state and does nothing. A
    /// child of it is where a registered panel's window lives.
    unsafe fn container_for_test() -> HWND {
        unsafe {
            register_dock_classes();
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                DOCK_GROUP_CLASS,
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(GetModuleHandleW(None).unwrap_or_default().into()),
                None,
            )
            .expect("dock group window")
        }
    }

    /// A hidden window of a system class: a child of `parent`, or a
    /// top-level one of the "plugin's own".
    unsafe fn plain_window_for_test(parent: Option<HWND>) -> HWND {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("Static"),
                PCWSTR::null(),
                if parent.is_some() { WS_CHILD } else { WS_POPUP },
                0,
                0,
                10,
                10,
                parent,
                None,
                None,
                None,
            )
            .expect("plain window")
        }
    }

    /// The rule itself, on real windows: a registration stands while
    /// its window is a direct child of a dock group or of the main
    /// window, and ends when the plugin moves it anywhere else, wraps
    /// it, or destroys it.
    #[test]
    fn a_registration_stands_only_inside_a_host_container() {
        unsafe extern "system" fn plain_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        unsafe {
            let group = container_for_test();
            // A stand-in for the main window: only its class matters
            // to the rule, and nothing else in the test process
            // registers that class.
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let class = WNDCLASSEXW {
                cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap_or(0),
                lpfnWndProc: Some(plain_proc),
                hInstance: instance.into(),
                lpszClassName: MAIN_CLASS,
                ..Default::default()
            };
            let _ = RegisterClassExW(&raw const class);
            let main = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                MAIN_CLASS,
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .expect("stand-in main window");
            let elsewhere = plain_window_for_test(None);
            let client = plain_window_for_test(Some(group));

            assert!(registration_stands(client), "docked in a group");
            let _ = SetParent(client, Some(main));
            assert!(registration_stands(client), "parked under the main window");

            let _ = SetParent(client, Some(elsewhere));
            assert!(
                !registration_stands(client),
                "moved into a window of the plugin's own"
            );
            let _ = SetParent(client, None);
            assert!(!registration_stands(client), "made top-level");
            let _ = SetParent(client, Some(group));
            assert!(
                registration_stands(client),
                "back in a group, it stands again"
            );

            // Wrapped: inside a window that is itself inside the
            // group. The wrapper is what the group would lay out, and
            // it is not what was registered.
            let wrapper = plain_window_for_test(Some(group));
            let _ = SetParent(client, Some(wrapper));
            assert!(!registration_stands(client), "inside a wrapper");
            assert!(registration_stands(wrapper));

            let _ = DestroyWindow(wrapper);
            assert!(!registration_stands(client), "destroyed with its wrapper");
            assert!(!registration_stands(wrapper), "destroyed");
            assert!(!registration_stands(HWND::default()), "no handle at all");

            // The containers themselves are not panels: a group is a
            // popup or a child of the main window, the main window a
            // top-level one.
            assert!(!registration_stands(group));
            assert!(!registration_stands(main));

            // Nor is any window of the host's own classes, wherever it
            // is: a docked group is a direct child of the main window,
            // and a registration whose handle value Windows gave to one
            // must not have the host lay its own chrome out as a panel.
            let docked_group = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                DOCK_GROUP_CLASS,
                PCWSTR::null(),
                WS_CHILD,
                0,
                0,
                0,
                0,
                Some(main),
                None,
                Some(instance.into()),
                None,
            )
            .expect("docked group window");
            assert!(
                !registration_stands(docked_group),
                "one of the host's own windows read as a plugin's panel"
            );

            let _ = DestroyWindow(group);
            let _ = DestroyWindow(main);
            let _ = DestroyWindow(elsewhere);
        }
    }

    /// Two plugin panels and three handle values for the retirement
    /// tests, which name no window: `stands` is a closure over them.
    fn retirement_fixture() -> (DockPanel, DockPanel, HWND, HWND, HWND) {
        (
            codepp_core::dock::intern_plugin_panel("rt.dll", "Retired Panel").expect("intern"),
            codepp_core::dock::intern_plugin_panel("rt.dll", "Standing Panel").expect("intern"),
            HWND(0x1001 as *mut c_void),
            HWND(0x1002 as *mut c_void),
            HWND(0x1003 as *mut c_void),
        )
    }

    /// A registration whose window has left is dropped and its panel
    /// closed, with its place remembered; one that stands is not
    /// touched, and a second pass finds nothing to do.
    #[test]
    fn a_departed_registration_is_dropped_and_its_panel_closed() {
        let (gone, kept, h_gone, h_kept, _) = retirement_fixture();
        let mut dialogs = vec![entry_for_test(gone, h_gone), entry_for_test(kept, h_kept)];
        let mut l = DockLayout::new();
        l.set_initial_side(gone, DockSide::Bottom);
        l.set_initial_side(kept, DockSide::Bottom);
        l.show(gone);
        l.show(kept);
        let before = l.container_of(gone);

        // Every window standing: nothing happens.
        assert_eq!(
            retire_departed(&mut dialogs, &mut l, None, |_| true),
            Retired::default()
        );
        assert_eq!(dialogs.len(), 2);

        let retired = retire_departed(&mut dialogs, &mut l, None, |h| h != h_gone);
        assert_eq!(
            retired,
            Retired {
                registrations: 1,
                closed: vec![gone],
            }
        );
        assert_eq!(dialogs.len(), 1);
        assert_eq!(dialogs[0].panel, kept, "the standing registration stays");
        assert!(!l.is_visible(gone), "the departed panel is closed");
        assert!(l.is_visible(kept), "the standing panel stays open");

        assert_eq!(
            retire_departed(&mut dialogs, &mut l, None, |h| h != h_gone),
            Retired::default(),
            "nothing is left to retire"
        );

        // Registered again and shown, the panel is back where it was.
        assert!(
            before.is_same(l.container_of(gone)),
            "the layout forgot where the retired panel was"
        );
    }

    /// A closed panel has nothing to close, and a parked one is closed
    /// rather than saved as open with nothing registered for it.
    #[test]
    fn retiring_closes_a_parked_panel_and_leaves_a_closed_one() {
        let (hidden, parked, h_hidden, h_parked, _) = retirement_fixture();
        let mut dialogs = vec![
            entry_for_test(hidden, h_hidden),
            entry_for_test(parked, h_parked),
        ];
        let mut l = DockLayout::new();
        l.show(parked);
        assert!(l.park(&[parked]), "precondition: the panel parks");
        assert!(l.is_parked(parked) && !l.is_visible(parked));

        let retired = retire_departed(&mut dialogs, &mut l, None, |_| false);
        assert_eq!(retired.registrations, 2);
        assert_eq!(retired.closed, vec![parked]);
        assert!(dialogs.is_empty());
        assert!(!l.is_parked(parked), "a retired panel stayed parked");
        assert!(!l.is_visible(parked) && !l.is_visible(hidden));
    }

    /// The panel a replacement holds, or is about to hold, stays open:
    /// a plugin that destroys its window and registers a new one under
    /// the same name keeps its panel on screen. Two ended registrations
    /// for one panel close it once.
    #[test]
    fn a_replaced_panel_stays_open_through_the_retirement() {
        let (panel, other, h_old, h_new, h_older) = retirement_fixture();

        // `NPPM_DMMREGASDCKDLG`'s case: the replacement is not in the
        // list yet, and names the panel to keep.
        let mut dialogs = vec![entry_for_test(panel, h_old), entry_for_test(other, h_older)];
        let mut l = DockLayout::new();
        l.show(panel);
        l.show(other);
        let retired = retire_departed(&mut dialogs, &mut l, Some(panel), |_| false);
        assert_eq!(retired.registrations, 2);
        assert_eq!(
            retired.closed,
            vec![other],
            "only the panel nobody replaces"
        );
        assert!(l.is_visible(panel), "the panel being re-registered closed");
        assert!(!l.is_visible(other));

        // A replacement already standing in the list.
        let mut dialogs = vec![entry_for_test(panel, h_old), entry_for_test(panel, h_new)];
        let mut l = DockLayout::new();
        l.show(panel);
        let retired = retire_departed(&mut dialogs, &mut l, None, |h| h == h_new);
        assert_eq!(retired.registrations, 1);
        assert!(retired.closed.is_empty());
        assert!(l.is_visible(panel));
        assert_eq!(dialogs.len(), 1);
        assert_eq!(dialogs[0].h_client, h_new);

        // Two ended registrations for one panel.
        let mut dialogs = vec![entry_for_test(panel, h_old), entry_for_test(panel, h_older)];
        let retired = retire_departed(&mut dialogs, &mut l, None, |_| false);
        assert_eq!(retired.registrations, 2);
        assert_eq!(retired.closed, vec![panel], "closed once, not twice");
    }

    /// A tab drag that ends moves only a panel still open. The second
    /// half is why the guard exists: `move_panel` reopens a closed one.
    #[test]
    fn a_drop_does_not_put_back_a_panel_closed_during_the_drag() {
        let (panel, _, _, _, _) = retirement_fixture();
        let mut l = DockLayout::new();
        l.show(panel);
        assert!(drop_moves_panel(&l, panel));
        l.hide(panel);
        assert!(!drop_moves_panel(&l, panel));

        l.move_panel(panel, DropTarget::Side(DockSide::Left));
        assert!(
            l.is_visible(panel),
            "precondition: move_panel no longer reopens a closed panel, so the guard is moot"
        );
    }

    /// Whitespace removed, so a guard pins statements and not the
    /// layout rustfmt happens to give them.
    fn squashed(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// The body of `name` in this file's production half, comments
    /// stripped and whitespace removed.
    fn own_body(name: &str) -> String {
        use crate::plugin_reentry_guards::{code_only, fn_body};
        let src = include_str!("dock_panels.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("test module")];
        squashed(&code_only(&fn_body(src, name)))
    }

    /// The same for a function of `lib.rs`.
    fn lib_body(name: &str) -> String {
        use crate::plugin_reentry_guards::{code_only, fn_body, production_src};
        squashed(&code_only(&fn_body(production_src(), name)))
    }

    /// Where `needle`, written with ordinary spacing, starts in a
    /// squashed body.
    fn at(body: &str, needle: &str) -> Option<usize> {
        body.find(&squashed(needle))
    }

    /// The reconciler never takes a plugin's window back. Orderings a
    /// unit test cannot see hold that: ended registrations are retired
    /// before the model is read; the snapshot lists only windows whose
    /// registration stands; and each is asked again right before every
    /// call that re-parents, shows or hides it — after the previous
    /// call too, since each of those calls sends the window a message
    /// its plugin may answer by taking it away.
    #[test]
    fn the_reconciler_leaves_a_departed_window_alone() {
        let apply = own_body("reconcile_tree");

        let quit_check = at(&apply, "if !crate::quitting() {")
            .expect("retirement must stand down once the quit has begun");
        let retire = at(
            &apply,
            "retire_departed(&mut state.dock_dialogs, &mut state.dock_layout, None, \
             registration_stands",
        )
        .expect("the reconcile no longer retires ended registrations");
        let snapshot =
            at(&apply, "state.dock_layout.clone()").expect("the reconcile's snapshot of the model");
        assert!(
            quit_check < retire && retire < snapshot,
            "ended registrations must be retired, outside a quit, before the model is read"
        );
        assert!(
            at(
                &apply,
                ".filter(|e| registration_stands(e.h_client)).map(|e| (e.panel, e.h_client))"
            )
            .is_some(),
            "the snapshot must list only windows whose registration stands"
        );

        let ask = at(&apply, "if !still_ours(*panel, *panel_hwnd) { continue; }")
            .expect("a shown panel's window is no longer asked before it is re-parented");
        let into_group =
            at(&apply, "SetParent(*panel_hwnd, Some(ghwnd))").expect("the re-parent into a group");
        assert!(ask < into_group);
        assert!(
            at(
                &apply[into_group..],
                "SetParent(*panel_hwnd, Some(ghwnd)); \
                 if !still_ours(*panel, *panel_hwnd) { continue; } } \
                 let _ = ShowWindow(*panel_hwnd,"
            ) == Some(0),
            "a window re-parented into a group is shown or hidden without being asked again"
        );
        let hidden = at(
            &apply,
            "if !layout.is_visible(panel) && still_ours(panel, panel_hwnd) { \
             let _ = ShowWindow(panel_hwnd, SW_HIDE); \
             if still_ours(panel, panel_hwnd) \
             && GetParent(panel_hwnd).unwrap_or_default() != main_hwnd { \
             let _ = SetParent(panel_hwnd, Some(main_hwnd)); }",
        )
        .expect(
            "a hidden panel's window must be asked before the hide and again before it is parked",
        );
        assert!(into_group < hidden);
        assert!(
            at(
                &apply,
                "if GetParent(panel_hwnd).unwrap_or_default() == gw.hwnd { \
                 let _ = ShowWindow(panel_hwnd, SW_HIDE); \
                 if GetParent(panel_hwnd).unwrap_or_default() == gw.hwnd { \
                 let _ = SetParent(panel_hwnd, Some(main_hwnd)); } }"
            )
            .is_some(),
            "a dying group's content must be moved out only while it is still that group's"
        );
        assert!(
            at(
                &apply,
                "!matches!(panel, DockPanel::Plugin(_)) || registration_stands(hwnd)"
            )
            .is_some(),
            "`still_ours` no longer asks whether a plugin's registration stands"
        );
        assert_eq!(
            apply.matches("SetParent(").count(),
            3,
            "a new re-parent in the reconciler: it must ask `still_ours` like the others"
        );

        // What a group lays out is asked the same way.
        assert!(
            at(
                &own_body("panel_content_hwnd"),
                ".find(|e| e.panel == panel && registration_stands(e.h_client))"
            )
            .is_some(),
            "a group would size and move a window that has left"
        );
    }

    /// A group that notices a window gone only asks; the reconcile
    /// runs from the message loop. From `WM_PAINT` it would destroy
    /// the window being painted, and `WM_PARENTNOTIFY` arrives while
    /// the window still exists. And nothing is asked, or done, once
    /// the quit has begun.
    #[test]
    fn a_group_asks_for_the_sweep_and_never_reconciles_itself() {
        let paint = own_body("paint_group");
        let departed = at(
            &paint,
            ".any(|e| e.panel == front && !registration_stands(e.h_client))",
        )
        .expect("a group painting no longer looks for a front panel whose window has left");
        let end_paint = at(&paint, "EndPaint(").expect("the paint");
        let ask = at(
            &paint,
            "if let Some(main) = sweep_from { request_departure_sweep(main); }",
        )
        .expect("a group painting an empty band no longer asks for the sweep");
        assert!(
            departed < end_paint && end_paint < ask,
            "the sweep is asked for mid-paint"
        );
        assert!(
            !paint.contains("apply_dock_layout("),
            "a group reconciles from inside its own WM_PAINT"
        );

        let proc = own_body("dock_group_wnd_proc");
        let arm = at(&proc, "WM_PARENTNOTIFY => {").expect("a group no longer hears of a child");
        let next = at(&proc[arm..], "WM_LBUTTONDOWN =>").expect("next arm");
        let arm = &proc[arm..arm + next];
        assert!(
            at(
                arm,
                "if (wparam.0 & 0xFFFF) as u32 == WM_DESTROY \
                 && GetAncestor(HWND(lparam.0 as *mut c_void), GA_PARENT) == hwnd { \
                 if let Some(main) = find_main_hwnd(hwnd) { request_departure_sweep(main); } }"
            )
            .is_some(),
            "the destruction of a child of the group, and only that, must ask for the sweep"
        );
        assert!(!arm.contains("apply_dock_layout("));

        let request = own_body("request_departure_sweep");
        assert!(
            at(
                &request,
                "if crate::quitting() || SWEEP_POSTED.with(|p| p.replace(true)) { return; }"
            )
            .is_some(),
            "the request must stand down during a quit, and be made once"
        );
        assert!(request.contains("PostMessageW(") && !request.contains("apply_dock_layout("));
        assert!(
            at(
                &request,
                "if posted.is_err() { SWEEP_POSTED.with(|p| p.set(false)); }"
            )
            .is_some(),
            "a request that could not be posted would block every later one"
        );

        let sweep = own_body("sweep_departed_plugin_panels");
        let clear = at(&sweep, "SWEEP_POSTED.with(|p| p.set(false));")
            .expect("the sweep no longer clears its flag");
        let quit_check = at(&sweep, "if crate::quitting() { return; }")
            .expect("the sweep must do nothing once the quit has begun");
        let reconcile = at(
            &sweep,
            "if departed { unsafe { apply_dock_layout(main_hwnd) }; }",
        )
        .expect("the sweep must reconcile only when a registration has ended");
        assert!(clear < quit_check && quit_check < reconcile);
        assert!(
            at(&sweep, ".any(|e| !registration_stands(e.h_client))").is_some(),
            "the sweep no longer looks for an ended registration"
        );

        // And the main window answers the message with the sweep.
        use crate::plugin_reentry_guards::{code_only, production_src};
        let lib = squashed(&code_only(production_src()));
        assert!(
            at(
                &lib,
                "WM_APP_DOCK_SWEEP => { dock_panels::sweep_departed_plugin_panels(hwnd); LRESULT(0) }"
            )
            .is_some(),
            "nothing answers the sweep a group asks for"
        );
    }

    /// Registration is the other place an ended registration is
    /// dropped: only standing ones can refuse a new one, and the
    /// retirement runs once the registration is going ahead — after the
    /// `SetParent` that makes the window stand, which is why it does
    /// not count the window being registered — keeping its panel open.
    #[test]
    fn registration_counts_only_standing_registrations_and_retires_the_rest() {
        let register = lib_body("register_dock_dialog");
        let quit = at(&register, "if quitting() {").expect(
            "a registration during a quit would retire, and close panels the user had open",
        );
        assert!(
            quit < at(&register, "IsWindow(Some(h_client))").expect("the first window check"),
            "the quit check must come before anything is looked at"
        );
        assert!(
            at(
                &register,
                "let standing = |e: &&DockEntry| dock_panels::registration_stands(e.h_client);"
            )
            .is_some(),
            "registration no longer tells standing registrations from ended ones"
        );
        for check in [
            "if dialogs.iter().filter(standing).count() >= DOCK_DIALOG_REGISTRATION_CAP {",
            "if dialogs.iter().filter(standing).any(|e| e.panel == panel) {",
        ] {
            assert!(
                at(&register, check).is_some(),
                "an ended registration can refuse a new one: `{check}` is gone"
            );
        }
        assert_eq!(
            register.matches("dialogs.iter()").count(),
            2,
            "a check in registration reads the list without asking which registrations stand"
        );
        let adopt = at(
            &register,
            "if SetParent(h_client, Some(main_hwnd_owner)).is_err() {",
        )
        .expect("the adoption");
        // A window whose registration stands is not registered a second
        // time: the registration it has is refreshed, and the function
        // answers before anything is counted, adopted, restyled or
        // hidden — the panel is on screen, or wherever it was.
        let refresh = at(
            &register,
            "if let Some(entry) = dialogs.iter_mut().find(|e| { \
             e.h_client.0 == h_client.0 && dock_panels::registration_stands(e.h_client) }) { \
             let (panel, u_mask) = (entry.panel, params.u_mask); \
             let tell_again = !dock_panels::delivering_dock_notices(); \
             if !refresh_dock_registration(entry, params, tell_again) { return None; }",
        )
        .expect("a second registration of a standing window must refresh the first");
        assert_eq!(register.matches("dialogs.iter_mut()").count(), 1);
        // The mask's side is seeded again, and the layout marked for
        // the reconcile that repaints the tab and sends the notice.
        let answered = refresh
            + at(
                &register[refresh..],
                "if let Some(side) = codepp_plugin_host::docking::dock_side_from_u_mask(u_mask) { \
                 (*self.dock_layout).set_initial_side(panel, side); } \
                 *self.dock_dirty = true; return Some(panel); }",
            )
            .expect("the refresh must seed the side, mark the layout and answer with its panel");
        assert!(
            at(&register, "caller: params.caller,").is_some(),
            "a registration no longer records which plugin it was taken for"
        );
        let cap = at(&register, "DOCK_DIALOG_REGISTRATION_CAP {").expect("the cap");
        assert!(
            answered < cap && cap < adopt,
            "a refresh must answer before the cap is counted and the window adopted"
        );
        for touches in [
            "SetParent(",
            "SetWindowLongPtrW(",
            "ShowWindow(",
            ".unpark(",
        ] {
            assert!(
                at(&register[refresh..answered], touches).is_none(),
                "a refresh moved or restyled a docked panel: `{touches}`"
            );
        }
        let retire = at(
            &register,
            "dock_panels::retire_departed(dialogs, &mut *self.dock_layout, Some(panel), |h|",
        )
        .expect("registration must drop ended registrations and keep its own panel open");
        let push = at(&register, "dialogs.push(DockEntry {").expect("the push");
        assert!(
            adopt < retire && retire < push,
            "ended registrations are dropped once the window is adopted, before it is listed"
        );
        assert!(
            at(
                &register[retire..push],
                "h != h_client && dock_panels::registration_stands(h)"
            )
            .is_some(),
            "the window being registered must not count as a standing registration"
        );
    }

    /// Every lookup a plugin's message makes, and everything the host
    /// sends a panel's window, go by registrations that stand. And a
    /// drop moves only a panel still open.
    #[test]
    fn plugin_messages_find_only_standing_registrations() {
        for (function, needle) in [
            (
                "panel_for_client",
                "if !dock_panels::registration_stands(h) { return None; }",
            ),
            (
                "update_dock_disp_info",
                "if !dock_panels::registration_stands(h) { return false; }",
            ),
            (
                "view_other_dock_tab",
                ".find(|e| e.name == name && dock_panels::registration_stands(e.h_client))",
            ),
            (
                "dock_hwnd_by_module",
                ") && dock_panels::registration_stands(entry.h_client)",
            ),
            (
                "hide_plugin_panel",
                "let target = target.filter(|h| dock_panels::registration_stands(*h)); \
                 if let Some(h_client) = target {",
            ),
        ] {
            assert!(
                at(&lib_body(function), needle).is_some(),
                "`{function}` finds a registration whose window has left"
            );
        }
        // A dock notice goes only to a registration that stands, which
        // `dock_notices_go_out_through_one_checked_sender` pins.
        assert!(
            at(
                &own_body("apply_requested_panel_focus"),
                ".find(|e| e.panel == panel && registration_stands(e.h_client))"
            )
            .is_some(),
            "focus goes to a window that has left"
        );
        let up = own_body("on_group_button_up");
        assert!(
            at(
                &up,
                "if drop_moves_panel(&state.dock_layout, panel) { \
                 state.dock_layout.move_panel(panel, target); }"
            )
            .is_some(),
            "a drop puts back a panel closed during the drag"
        );
        assert_eq!(up.matches("move_panel(").count(), 1);
    }

    /// A reconcile asked for from inside one is owed, not run: the
    /// running one makes another pass, and stops after a fixed number
    /// when every pass asks again. Nested, a plugin that rebuilds its
    /// panel whenever the panel is shown would recurse the host until
    /// its stack ran out.
    #[test]
    fn a_reconcile_asked_for_during_one_is_owed_not_nested() {
        use std::cell::Cell;

        // Three passes are each answered with a request; the fourth
        // settles.
        let runs = Cell::new(0u32);
        let passes = run_reconcile(|| {
            runs.set(runs.get() + 1);
            if runs.get() <= 3 {
                let nested = run_reconcile(|| panic!("a nested reconcile ran"));
                assert_eq!(nested, None, "the nested call must only note what is owed");
            }
        });
        assert_eq!(passes, Some(4));
        assert_eq!(runs.get(), 4);

        // Every pass answered with a request: the bound.
        let runs = Cell::new(0u32);
        let passes = run_reconcile(|| {
            runs.set(runs.get() + 1);
            let _ = run_reconcile(|| panic!("a nested reconcile ran"));
        });
        assert_eq!(passes, Some(MAX_RECONCILE_PASSES));
        assert_eq!(runs.get(), MAX_RECONCILE_PASSES);

        // Nothing is left owed by that: the next reconcile runs once.
        assert_eq!(run_reconcile(|| {}), Some(1));

        // And an unwind out of a pass releases the latch.
        let caught = std::panic::catch_unwind(|| run_reconcile(|| panic!("mid-pass")));
        assert!(caught.is_err(), "precondition: the pass unwound");
        assert_eq!(
            run_reconcile(|| {}),
            Some(1),
            "the latch survived the unwind"
        );
    }

    /// The reconciler's passes run only under the latch, and the
    /// notifications of phase 6 go out once it is released, so a
    /// handler that shows or hides a panel still reconciles at once.
    #[test]
    fn a_pass_runs_only_under_the_latch_and_notices_follow_it() {
        let apply = own_body("apply_dock_layout");
        let passes = at(
            &apply,
            "if run_reconcile(|| unsafe { reconcile_tree(main_hwnd) }).is_none() { return; }",
        )
        .expect("the reconcile no longer runs its passes under the latch");
        let notices =
            at(&apply, "unsafe { notify_plugin_panels(main_hwnd) };").expect("the notices");
        assert!(
            passes < notices,
            "the notices are sent before the passes are over"
        );
        let src = include_str!("dock_panels.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("test module")];
        assert_eq!(
            squashed(&crate::plugin_reentry_guards::code_only(src))
                .matches("reconcile_tree(")
                .count(),
            2,
            "a pass is run from somewhere other than `apply_dock_layout`, outside the latch"
        );
        let run = own_body("run_reconcile");
        assert!(
            at(
                &run,
                "let Some(_running) = Reconciling::enter() else { \
                 RECONCILE_OWED.with(|o| o.set(true)); return None; };"
            )
            .is_some(),
            "a nested reconcile must note that a pass is owed, and run nothing"
        );
        assert!(
            at(
                &own_body("reconcile_tree"),
                "unsafe { SWEEP_POSTED.with(|p| p.set(false));"
            )
            .is_some(),
            "a pass no longer frees the sweep latch, which a lost message would leave set"
        );
    }

    /// A dropped registration's tab icon is the host's own bitmap and
    /// is deleted exactly once: by the retirement, or at teardown,
    /// which takes it out of the entry so a later retirement finds
    /// none.
    #[test]
    fn a_tab_icon_is_deleted_once() {
        use crate::plugin_reentry_guards::{code_only, production_src};
        assert!(
            at(
                &own_body("retire_departed"),
                "if let Some(bmp) = entry.tab_icon { let _ = unsafe { DeleteObject(bmp.into()) }; }"
            )
            .is_some(),
            "a retired registration leaks its tab icon"
        );
        assert!(
            at(
                &squashed(&code_only(production_src())),
                "for entry in &mut state.dock_dialogs { \
                 if let Some(bmp) = entry.tab_icon.take() { let _ = DeleteObject(bmp.into()); } }"
            )
            .is_some(),
            "teardown must take each icon out of its entry as it deletes it"
        );
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

    /// The latch admits one focus move per thread at a time and clears
    /// when the move ends, the unwinding path included.
    #[test]
    fn the_focus_latch_admits_one_move_and_clears() {
        let outer = PanelFocusing::enter().expect("no move under way");
        assert!(
            PanelFocusing::enter().is_none(),
            "a nested move was admitted"
        );
        drop(outer);
        assert!(PanelFocusing::enter().is_some(), "the latch did not clear");
        let caught = std::panic::catch_unwind(|| {
            let _focusing = PanelFocusing::enter();
            panic!("inside a move");
        });
        assert!(caught.is_err());
        assert!(
            PanelFocusing::enter().is_some(),
            "an unwinding move left the latch set"
        );
    }

    /// The move takes the latch before it focuses anything, and drops the
    /// request when the latch is taken: the order that keeps a plugin's
    /// `WM_SETFOCUS` handler from recursing through `NPPM_DMMSHOW`.
    #[test]
    fn the_focus_move_is_latched_before_it_focuses() {
        use crate::plugin_reentry_guards::{code_without_strings, fn_body};
        let src = include_str!("dock_panels.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("test module")];
        let body = code_without_strings(&fn_body(src, "apply_requested_panel_focus"));
        let take = body
            .find("PANEL_FOCUS.with(std::cell::Cell::take)")
            .expect("the move no longer takes the request");
        let latch = body
            .find("let Some(_focusing) = PanelFocusing::enter() else {")
            .expect("the move no longer takes the latch, or ignores it");
        let focus = body
            .find("SetFocus(Some(client))")
            .expect("the move no longer focuses the panel");
        assert!(
            take < latch && latch < focus,
            "the request must be taken, then the latch, then the focus moved"
        );
    }

    /// `return_focus` gives focus back to the window it was given when
    /// that window is shown and focus is elsewhere or nowhere, and not
    /// when it is not shown or has focus already. No windows: the
    /// handles are only compared, so this runs the same on every runner.
    #[test]
    fn focus_goes_back_only_to_a_shown_window_without_it() {
        let before = HWND(0x1010 as *mut c_void);
        let elsewhere = HWND(0x2020 as *mut c_void);
        for now in [elsewhere, HWND::default()] {
            assert_eq!(
                focus_to_return(before, true, now),
                Some(before),
                "focus did not go back to the window it was before"
            );
            assert_eq!(
                focus_to_return(before, false, now),
                None,
                "a window that is not shown took focus"
            );
        }
        assert_eq!(
            focus_to_return(before, true, before),
            None,
            "focus was moved onto the window that has it"
        );
    }

    /// `return_focus` decides with the real queries: the two tests that
    /// follow check `focus_to_return` and `window_is_shown` apart, and
    /// neither would notice a call site that passed a constant instead.
    #[test]
    fn return_focus_decides_with_the_window_and_focus_it_reads() {
        use crate::plugin_reentry_guards::{code_without_strings, fn_body};
        let src = include_str!("dock_panels.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("test module")];
        // Whitespace and a trailing comma dropped on both sides, so a
        // rustfmt rewrap of the call, which adds the comma, does not fail
        // the guard.
        let squash = |s: &str| s.split_whitespace().collect::<String>().replace(",)", ")");
        let body = squash(&code_without_strings(&fn_body(src, "return_focus")));
        assert!(
            body.contains(&squash(
                "focus_to_return(before, window_is_shown(before), focus_now())"
            )),
            "return_focus must decide on whether `before` is shown and where focus is now"
        );
    }

    /// `window_is_shown`, which decides whether `return_focus` may give
    /// focus back, refuses a null handle — the startup case — a
    /// destroyed window and a hidden one, and accepts a shown one.
    ///
    /// The shown case is checked only where the desktop window reads as
    /// shown. `IsWindowVisible` walks a window's ancestors up to and
    /// including the desktop window, and on the Windows CI runner, which
    /// runs as a Windows service, it answered FALSE for a window that,
    /// like its top-level parent, had `WS_VISIBLE`: by elimination no
    /// window there reads as shown. A null desktop handle turns the
    /// check off too, since `IsWindowVisible` answers FALSE for it. The
    /// decision built on this is tested without windows above, on every
    /// runner.
    #[test]
    fn a_window_is_shown_unless_it_is_null_destroyed_or_hidden() {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetAncestor, GA_PARENT, GWL_STYLE, WINDOW_STYLE, WS_VISIBLE,
        };
        unsafe {
            let instance = GetModuleHandleW(None).unwrap_or_default();
            // Off screen, and never activated by a click or listed in
            // the taskbar, so a test run shows nothing.
            let edit = |parent: Option<HWND>, visible: bool| {
                let shown = if visible {
                    WS_VISIBLE
                } else {
                    WINDOW_STYLE::default()
                };
                CreateWindowExW(
                    if parent.is_some() {
                        WINDOW_EX_STYLE::default()
                    } else {
                        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
                    },
                    w!("EDIT"),
                    PCWSTR::null(),
                    shown | if parent.is_some() { WS_CHILD } else { WS_POPUP },
                    if parent.is_some() { 0 } else { -32000 },
                    if parent.is_some() { 0 } else { -32000 },
                    50,
                    20,
                    parent,
                    None,
                    Some(instance.into()),
                    None,
                )
                .expect("edit control")
            };
            let top = edit(None, true);
            let shown = edit(Some(top), true);
            let hidden = edit(Some(top), false);
            let gone = edit(Some(top), true);
            let _ = DestroyWindow(gone);

            assert!(
                !window_is_shown(HWND::default()),
                "a null handle read as shown"
            );
            assert!(!window_is_shown(gone), "a destroyed window read as shown");
            assert!(!window_is_shown(hidden), "a hidden window read as shown");

            let desktop = GetAncestor(top, GA_PARENT);
            if IsWindowVisible(desktop).as_bool() {
                let style = |w: HWND| GetWindowLongPtrW(w, GWL_STYLE);
                assert!(
                    window_is_shown(shown),
                    "a shown window read as hidden; styles: desktop {:#010x}, \
                     top {:#010x}, window {:#010x}",
                    style(desktop),
                    style(top),
                    style(shown),
                );
            }

            let _ = DestroyWindow(top);
        }
    }
}
