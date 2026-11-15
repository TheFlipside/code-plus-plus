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
    GetAncestor, PostMessageW, GA_PARENT, WM_DESTROY, WM_PARENTNOTIFY,
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
/// always has, and `deliver_container_notices` bounds that round trip
/// with a queue of its own.
pub(crate) unsafe fn apply_dock_layout(main_hwnd: HWND) {
    // SAFETY: the caller's contract, passed on to each pass.
    if run_reconcile(|| unsafe { reconcile_tree(main_hwnd) }).is_none() {
        return;
    }
    // Phase 6 (borrow, then none): tell each plugin whose panel
    // changed container, and each newly registered one where its
    // panel lives. Last, so a plugin that reacts by querying the
    // host or the window tree finds both already settled — and
    // with no borrow held for the send, so its handler's
    // `NPPM_*` is answered rather than declined.
    unsafe {
        let notices = state_from_hwnd(main_hwnd)
            .map(|state| container_notices(&state.dock_layout, &mut state.dock_dialogs))
            .unwrap_or_default();
        deliver_container_notices(main_hwnd, notices);
    }
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
            out.push((
                entry.h_client,
                codepp_plugin_host::docking::dock_container_code(layout, now),
            ));
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
    // Nothing once the quit has begun: the plugins have heard, or are
    // hearing, that the host is shutting down.
    if crate::quitting() {
        return;
    }
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
        // A handler for an earlier notice began the quit: the plugins
        // are being told the host is shutting down, and hear nothing
        // of the dock after that. What is still queued goes with it;
        // its entries are window handles, and nothing will send them.
        if crate::quitting() {
            CONTAINER_NOTICES.with(|q| q.borrow_mut().clear());
            break;
        }
        // A handler for an earlier notice may have destroyed this
        // one's window, or taken it out of the host's containers,
        // which ends its registration; the record is already written,
        // so skipping it loses nothing that could still be delivered.
        if !registration_stands(h_client) {
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

/// Whether a `DMN_DOCK` / `DMN_FLOAT` delivery is running on this
/// thread, so that whatever asks is running inside a plugin's handler
/// for one. A registration refreshed from there is not told its
/// container again: the notice it would raise is queued behind the
/// handler, whose next run would raise another, and the drain would
/// not end while the plugin kept at it.
pub(crate) fn delivering_container_notices() -> bool {
    CONTAINER_NOTICES_DELIVERING.with(std::cell::Cell::get)
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
            caller: None,
        }
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
        let told = Some(codepp_core::dock::DockContainer::Docked(DockSide::Bottom));
        let mut entry = crate::DockEntry {
            tb_data: first,
            name: "Refresh A".into(),
            module_name: "RefreshTest.dll".into(),
            dlg_id: 3,
            dmn_container: told,
            caller: Some(2),
            ..entry_for_test(panel, h)
        };
        let untouched = |entry: &crate::DockEntry| {
            assert_eq!(entry.tb_data, first);
            assert_eq!(entry.name, "Refresh A");
            assert_eq!(entry.module_name, "RefreshTest.dll");
            assert_eq!((entry.dlg_id, entry.u_mask), (3, 0));
            assert_eq!(entry.dmn_container, told);
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

        // From inside a container notice's handler the registration is
        // refreshed, and what the plugin was told stands.
        let taken = unsafe {
            crate::refresh_dock_registration(
                &mut entry,
                params_for_test(h, "Refresh A", "RefreshTest.dll", first, Some(2)),
                false,
            )
        };
        assert!(taken);
        assert_eq!(entry.dmn_container, told);

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
        // Told again: the next reconcile finds nothing on record and
        // sends the registration's notice.
        assert_eq!(entry.dmn_container, None);
        let mut layout = DockLayout::new();
        layout.set_initial_side(panel, DockSide::Bottom);
        let notices = container_notices(&layout, std::slice::from_mut(&mut entry));
        assert_eq!(notices, vec![(h, (3 << 16) | codepp_plugin_host::DMN_DOCK)]);
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
                lpszClassName: w!("DockTestNoticeProbe"),
                ..Default::default()
            };
            let _ = RegisterClassExW(&raw const class);
            // Inside a dock group, as a registered panel's window is:
            // a notice goes only to a window whose registration stands.
            // Its class must not carry the host's own prefix either: a
            // window of a host class never stands.
            let container = container_for_test();
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("DockTestNoticeProbe"),
                PCWSTR::null(),
                WS_CHILD,
                0,
                0,
                0,
                0,
                Some(container),
                None,
                Some(instance.into()),
                None,
            )
            .expect("probe window");

            deliver_container_notices(HWND::default(), vec![(hwnd, 1)]);
            let _ = DestroyWindow(container);
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

    /// A delivery is known to be running exactly while one is: what a
    /// registration refreshed from a notice's handler goes by, so as
    /// not to be answered with another notice.
    #[test]
    fn a_delivery_is_known_to_be_running_only_while_it_is() {
        assert!(!delivering_container_notices());
        let delivering = NoticeDelivery::enter().expect("no delivery is running yet");
        assert!(delivering_container_notices());
        drop(delivering);
        assert!(!delivering_container_notices());
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
             let tell_again = !dock_panels::delivering_container_notices(); \
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
        let deliver = own_body("deliver_container_notices");
        let stands = at(&deliver, "if !registration_stands(h_client) { continue; }")
            .expect("a container notice goes to a window that has left");
        // And a delivery under way stops, and drops what it had
        // queued, once a handler has begun the quit.
        let quit = at(
            &deliver,
            "pop_front()) { if crate::quitting() { \
             CONTAINER_NOTICES.with(|q| q.borrow_mut().clear()); break; }",
        )
        .expect("a delivery under way goes on after the quit has begun");
        assert!(quit < stands);
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
        let notices = at(&apply, "deliver_container_notices(main_hwnd").expect("the notices");
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
