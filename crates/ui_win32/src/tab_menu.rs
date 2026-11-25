//! The tab strip's context menu on Win32.
//!
//! A right click on a tab brings it to the front, through the same path a
//! pick from the Window menu takes ([`super::handle_window_menu_click`]),
//! and then shows the menu `codepp_shell` builds for it
//! ([`Shell::tab_context_menu`](codepp_shell::Shell::tab_context_menu)).
//! The chosen entry is posted to the main window as a `WM_COMMAND`, so it
//! runs on a clean tick with no popup loop on the stack. Most entries post
//! the File-menu command they stand for; the rest are this module's
//! `ID_TAB_*` commands, which [`run_command`] carries out.

use std::ffi::c_void;

use codepp_shell::{ClipboardPart, TabColor, TabCommand, TabMenuItem};
use windows::core::HSTRING;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, PostMessageW, SetMenuItemInfoW, TrackPopupMenu,
    HMENU, MENUITEMINFOW, MENU_ITEM_FLAGS, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    MIIM_BITMAP, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_COMMAND,
};

use codepp_shell::CloseMultiKind;

use super::{
    active_tab_is, copy_text_to_clipboard, ensure_one_tab, fire_queued_notifications,
    force_tab_strip_resync, handle_close_active_tab, handle_window_menu_click, show_error_dialog,
    state_from_hwnd, update_window_title, DialogText, DrainFreeze, ID_FILE_CLOSE,
    ID_FILE_CLOSE_ALL_BUT_ACTIVE, ID_FILE_CLOSE_ALL_BUT_PINNED, ID_FILE_CLOSE_ALL_TO_LEFT,
    ID_FILE_CLOSE_ALL_TO_RIGHT, ID_FILE_CLOSE_ALL_UNCHANGED, ID_FILE_MOVE_TO_RECYCLE_BIN,
    ID_FILE_OPEN_CONTAINING_CMD, ID_FILE_OPEN_CONTAINING_EXPLORER,
    ID_FILE_OPEN_CONTAINING_POWERSHELL, ID_FILE_OPEN_CONTAINING_WORKSPACE,
    ID_FILE_OPEN_IN_DEFAULT_VIEWER, ID_FILE_PRINT, ID_FILE_RELOAD, ID_FILE_RENAME, ID_FILE_SAVE,
    ID_FILE_SAVE_AS, SCI_GETMODIFY,
};

/// Pin or unpin the active tab.
const ID_TAB_TOGGLE_PIN: u16 = 1070;
/// Copy the active tab's full path.
const ID_TAB_COPY_FULL_PATH: u16 = 1071;
/// Copy the active tab's file name.
const ID_TAB_COPY_FILE_NAME: u16 = 1072;
/// Copy the path of the active tab's folder.
const ID_TAB_COPY_DIR_PATH: u16 = 1073;
/// Move the active tab to the start of the strip.
const ID_TAB_MOVE_LEFTMOST: u16 = 1074;
/// Move the active tab to the end of the strip.
const ID_TAB_MOVE_RIGHTMOST: u16 = 1075;
/// Move to Other View. Not implemented: the entry is always greyed.
const ID_TAB_MOVE_OTHER_VIEW: u16 = 1076;
/// Clone to Other View. Not implemented: the entry is always greyed.
const ID_TAB_CLONE_OTHER_VIEW: u16 = 1077;
/// Open the active tab's file in a guest instance and close it here.
const ID_TAB_MOVE_NEW_INSTANCE: u16 = 1078;
/// Open the active tab's file in a guest instance as well.
const ID_TAB_OPEN_NEW_INSTANCE: u16 = 1079;
/// Apply Color 1; Color 2 to 5 follow it.
const ID_TAB_COLOR_BASE: u16 = 1080;
/// Apply Color 5, the last of the five.
const ID_TAB_COLOR_END: u16 = 1084;
/// Remove Color.
const ID_TAB_COLOR_REMOVE: u16 = 1085;

/// Side of the square drawn beside each Apply Color entry, in pixels.
const SWATCH_PX: i32 = 14;

/// The `WM_COMMAND` id an entry posts.
pub(crate) fn command_id(command: TabCommand) -> u16 {
    use TabCommand as C;
    match command {
        C::Close => ID_FILE_CLOSE,
        C::CloseMultiple(kind) => match kind {
            CloseMultiKind::AllButActive => ID_FILE_CLOSE_ALL_BUT_ACTIVE,
            CloseMultiKind::AllButPinned => ID_FILE_CLOSE_ALL_BUT_PINNED,
            CloseMultiKind::AllToLeft => ID_FILE_CLOSE_ALL_TO_LEFT,
            CloseMultiKind::AllToRight => ID_FILE_CLOSE_ALL_TO_RIGHT,
            CloseMultiKind::AllUnchanged => ID_FILE_CLOSE_ALL_UNCHANGED,
        },
        C::TogglePin => ID_TAB_TOGGLE_PIN,
        C::Save => ID_FILE_SAVE,
        C::SaveAs => ID_FILE_SAVE_AS,
        C::OpenContainingFileManager => ID_FILE_OPEN_CONTAINING_EXPLORER,
        C::OpenContainingTerminal => ID_FILE_OPEN_CONTAINING_CMD,
        C::OpenContainingPowerShell => ID_FILE_OPEN_CONTAINING_POWERSHELL,
        C::OpenContainingWorkspace => ID_FILE_OPEN_CONTAINING_WORKSPACE,
        C::OpenInDefaultViewer => ID_FILE_OPEN_IN_DEFAULT_VIEWER,
        C::Rename => ID_FILE_RENAME,
        C::MoveToRecycleBin => ID_FILE_MOVE_TO_RECYCLE_BIN,
        C::Reload => ID_FILE_RELOAD,
        C::Print => ID_FILE_PRINT,
        C::CopyFullPath => ID_TAB_COPY_FULL_PATH,
        C::CopyFileName => ID_TAB_COPY_FILE_NAME,
        C::CopyDirPath => ID_TAB_COPY_DIR_PATH,
        C::MoveToStart => ID_TAB_MOVE_LEFTMOST,
        C::MoveToEnd => ID_TAB_MOVE_RIGHTMOST,
        C::MoveToOtherView => ID_TAB_MOVE_OTHER_VIEW,
        C::CloneToOtherView => ID_TAB_CLONE_OTHER_VIEW,
        C::MoveToNewInstance => ID_TAB_MOVE_NEW_INSTANCE,
        C::OpenInNewInstance => ID_TAB_OPEN_NEW_INSTANCE,
        C::ApplyColor(color) => ID_TAB_COLOR_BASE + u16::from(color.number()) - 1,
        C::RemoveColor => ID_TAB_COLOR_REMOVE,
    }
}

/// Whether `id` is one of this module's commands, for [`run_command`].
pub(crate) fn is_tab_command(id: u16) -> bool {
    matches!(
        id,
        ID_TAB_TOGGLE_PIN
            | ID_TAB_COPY_FULL_PATH
            | ID_TAB_COPY_FILE_NAME
            | ID_TAB_COPY_DIR_PATH
            | ID_TAB_MOVE_LEFTMOST
            | ID_TAB_MOVE_RIGHTMOST
            | ID_TAB_MOVE_OTHER_VIEW
            | ID_TAB_CLONE_OTHER_VIEW
            | ID_TAB_MOVE_NEW_INSTANCE
            | ID_TAB_OPEN_NEW_INSTANCE
            | ID_TAB_COLOR_BASE..=ID_TAB_COLOR_END | ID_TAB_COLOR_REMOVE
    )
}

/// Win32's `COLORREF` (`0x00BBGGRR`) for a `0xRRGGBB` color.
pub(crate) fn colorref(rgb: u32) -> u32 {
    ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF)
}

/// Bring the tab at `idx` to the front and show its context menu at
/// `screen` coordinates. `main_hwnd` is the main window.
///
/// # Safety
///
/// Call on the UI thread, with no borrow of the window's state held.
pub(crate) unsafe fn show(main_hwnd: HWND, idx: usize, screen: POINT) {
    unsafe { handle_window_menu_click(main_hwnd, idx) };
    let items = {
        let Some(state) = (unsafe { state_from_hwnd(main_hwnd) }) else {
            return;
        };
        // The activation may not have happened (a plugin call holding the
        // state, a tab that went meanwhile): never show one tab's menu for
        // another.
        if state.shell.active_tab != Some(idx) {
            return;
        }
        let Some(tab) = state.shell.tabs.get(idx) else {
            return;
        };
        // As the close prompt reads it: the live bit, the cached one, and
        // the shell's record of a restored buffer that is not on disk.
        let dirty = tab.dirty
            || state.shell.is_unsaved_restore(tab.id)
            || state.editor.send(SCI_GETMODIFY, 0, 0) != 0;
        (tab.id, state.shell.tab_context_menu(idx, dirty))
    };
    let (tab_id, items) = items;
    let mut bitmaps = Vec::new();
    let Some(menu) = (unsafe { build_menu(&items, &mut bitmaps) }) else {
        return;
    };
    // `TPM_RETURNCMD` runs a modal loop: no state borrow is held here, and
    // the drain is frozen across it, as across the close prompt, so a
    // worker's result cannot move the tab in front, or change what the
    // menu greyed, while the menu is up.
    let freeze = DrainFreeze::new(main_hwnd);
    let cmd = unsafe {
        TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RETURNCMD | TPM_RIGHTBUTTON,
            screen.x,
            screen.y,
            Some(0),
            main_hwnd,
            None,
        )
    };
    unsafe {
        let _ = DestroyMenu(menu);
        for bitmap in bitmaps {
            let _ = DeleteObject(bitmap.into());
        }
    }
    // The choice is posted while the freeze still holds, and only if the
    // tab the menu was built for is still in front. Dropping the freeze
    // then re-posts any wake it held back, which therefore lands after the
    // command: nothing drained in between can move the tab the command
    // acts on, which is always the tab in front.
    if cmd.0 != 0 && unsafe { active_tab_is(main_hwnd, tab_id) } {
        let _ = unsafe {
            PostMessageW(
                Some(main_hwnd),
                WM_COMMAND,
                WPARAM(cmd.0 as usize),
                LPARAM(0),
            )
        };
    }
    drop(freeze);
}

/// A popup menu holding `items`, with the swatch bitmaps it uses pushed
/// onto `bitmaps` for the caller to delete once the menu is gone.
unsafe fn build_menu(items: &[TabMenuItem], bitmaps: &mut Vec<HBITMAP>) -> Option<HMENU> {
    let menu = unsafe { CreatePopupMenu() }.ok()?;
    for item in items {
        match item {
            TabMenuItem::Separator => unsafe {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            },
            TabMenuItem::Command {
                label,
                command,
                enabled,
                swatch,
            } => {
                let flags = MF_STRING | greyed(*enabled);
                let text = HSTRING::from(*label);
                let id = command_id(*command);
                let appended = unsafe { AppendMenuW(menu, flags, usize::from(id), &text) }.is_ok();
                if let Some(color) = swatch.filter(|_| appended) {
                    if let Some(bitmap) = unsafe { swatch_bitmap(color.bar_rgb()) } {
                        bitmaps.push(bitmap);
                        let info = MENUITEMINFOW {
                            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                            fMask: MIIM_BITMAP,
                            hbmpItem: bitmap,
                            ..Default::default()
                        };
                        // By command id: each entry's id is its own, so the
                        // square cannot land on another entry.
                        unsafe {
                            let _ = SetMenuItemInfoW(menu, u32::from(id), false, &raw const info);
                        }
                    }
                }
            }
            TabMenuItem::Submenu {
                label,
                enabled,
                items,
            } => {
                let Some(sub) = (unsafe { build_menu(items, bitmaps) }) else {
                    continue;
                };
                let text = HSTRING::from(*label);
                // The submenu is owned by `menu` once appended, and goes with
                // it; one that could not be appended is freed here.
                let appended = unsafe {
                    AppendMenuW(menu, MF_POPUP | greyed(*enabled), sub.0 as usize, &text)
                }
                .is_ok();
                if !appended {
                    unsafe {
                        let _ = DestroyMenu(sub);
                    }
                }
            }
        }
    }
    Some(menu)
}

fn greyed(enabled: bool) -> MENU_ITEM_FLAGS {
    if enabled {
        MENU_ITEM_FLAGS(0)
    } else {
        MF_GRAYED
    }
}

/// A [`SWATCH_PX`]-square 32-bit bitmap of `rgb` with a grey border, for
/// a menu entry's image, or `None` when GDI cannot make one.
unsafe fn swatch_bitmap(rgb: u32) -> Option<HBITMAP> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: SWATCH_PX,
            // Negative: top-down rows.
            biHeight: -SWATCH_PX,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap = unsafe {
        CreateDIBSection(
            None,
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        )
    }
    .ok()?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteObject(bitmap.into());
        }
        return None;
    }
    let n = (SWATCH_PX * SWATCH_PX) as usize;
    // SAFETY: a 32-bit DIB section of SWATCH_PX² pixels has `n` u32s at
    // `bits`, which stays valid while `bitmap` exists.
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u32>(), n) };
    let side = SWATCH_PX as usize;
    for (i, px) in pixels.iter_mut().enumerate() {
        let (x, y) = (i % side, i / side);
        let edge = x == 0 || y == 0 || x == side - 1 || y == side - 1;
        // BGRA in memory is 0xAARRGGBB as a little-endian u32: opaque.
        *px = 0xFF00_0000 | if edge { 0x0060_6060 } else { rgb & 0x00FF_FFFF };
    }
    Some(bitmap)
}

/// Carry out one of this module's commands on the active tab.
///
/// # Safety
///
/// Call from the main window's `WM_COMMAND`, on the UI thread, with no
/// borrow of the window's state held.
pub(crate) unsafe fn run_command(hwnd: HWND, id: u16) {
    match id {
        ID_TAB_TOGGLE_PIN => {
            let Some(state) = (unsafe { state_from_hwnd(hwnd) }) else {
                return;
            };
            let Some(idx) = state.shell.active_tab else {
                return;
            };
            let want = !state.shell.tabs.get(idx).is_some_and(|t| t.pinned);
            if state.shell.set_pinned(idx, want) {
                unsafe { force_tab_strip_resync(state) };
            }
            unsafe { fire_queued_notifications(hwnd) };
        }
        ID_TAB_COPY_FULL_PATH | ID_TAB_COPY_FILE_NAME | ID_TAB_COPY_DIR_PATH => {
            let part = match id {
                ID_TAB_COPY_FULL_PATH => ClipboardPart::FullPath,
                ID_TAB_COPY_FILE_NAME => ClipboardPart::FileName,
                _ => ClipboardPart::DirPath,
            };
            let text = unsafe { state_from_hwnd(hwnd) }.and_then(|state| {
                let idx = state.shell.active_tab?;
                state.shell.tab_clipboard_text(idx, part)
            });
            if let Some(text) = text {
                unsafe { copy_text_to_clipboard(hwnd, &text) };
            }
        }
        ID_TAB_MOVE_LEFTMOST | ID_TAB_MOVE_RIGHTMOST => {
            let Some(state) = (unsafe { state_from_hwnd(hwnd) }) else {
                return;
            };
            let Some(idx) = state.shell.active_tab else {
                return;
            };
            if state
                .shell
                .move_tab_to_edge(idx, id == ID_TAB_MOVE_RIGHTMOST)
            {
                unsafe { force_tab_strip_resync(state) };
            }
            unsafe { fire_queued_notifications(hwnd) };
        }
        ID_TAB_MOVE_NEW_INSTANCE | ID_TAB_OPEN_NEW_INSTANCE => unsafe {
            new_instance(hwnd, id == ID_TAB_MOVE_NEW_INSTANCE);
        },
        ID_TAB_COLOR_BASE..=ID_TAB_COLOR_END | ID_TAB_COLOR_REMOVE => {
            let color = if id == ID_TAB_COLOR_REMOVE {
                None
            } else {
                u8::try_from(id - ID_TAB_COLOR_BASE + 1)
                    .ok()
                    .and_then(TabColor::from_number)
            };
            let Some(state) = (unsafe { state_from_hwnd(hwnd) }) else {
                return;
            };
            let Some(idx) = state.shell.active_tab else {
                return;
            };
            if state.shell.set_tab_color(idx, color) {
                let tab_hwnd = state.tab_hwnd;
                unsafe {
                    let _ =
                        windows::Win32::Graphics::Gdi::InvalidateRect(Some(tab_hwnd), None, false);
                }
            }
        }
        // Move to Other View and Clone to Other View wait for a second
        // view; their entries are greyed, so nothing should post them.
        _ => {}
    }
}

/// Open the active tab's file in a guest instance
/// ([`codepp_shell::spawn_new_instance`]) and, for a move, close the tab
/// here once the new instance has started.
unsafe fn new_instance(hwnd: HWND, close_here: bool) {
    let path = {
        let Some(state) = (unsafe { state_from_hwnd(hwnd) }) else {
            return;
        };
        let Some(idx) = state.shell.active_tab else {
            return;
        };
        let Some(tab) = state.shell.tabs.get(idx) else {
            return;
        };
        let dirty = tab.dirty
            || state.shell.is_unsaved_restore(tab.id)
            || state.editor.send(SCI_GETMODIFY, 0, 0) != 0;
        let id = tab.id;
        state
            .shell
            .new_instance_path(idx, dirty)
            .map(|path| (id, path))
    };
    // The entry is greyed for a tab with no file or unsaved work, and a
    // posted command may arrive after either changed.
    let Some((id, path)) = path else {
        return;
    };
    if let Err(err) = codepp_shell::spawn_new_instance(&path) {
        tracing::warn!(?err, ?path, "a new instance could not be started");
        show_error_dialog(
            hwnd,
            "New Instance",
            &DialogText::paragraphs([
                "Code++ could not start a new instance.".to_string(),
                err.to_string(),
            ]),
        );
        return;
    }
    // Close only the tab whose file went, as the close prompt keys its
    // answer on the tab it asked about (`active_tab_is`).
    if close_here && unsafe { active_tab_is(hwnd, id) } {
        unsafe {
            handle_close_active_tab(hwnd);
            ensure_one_tab(hwnd);
        }
    }
    if let Some(state) = unsafe { state_from_hwnd(hwnd) } {
        unsafe { update_window_title(hwnd, &state.shell) };
    }
}

#[cfg(test)]
mod tests {
    use super::{colorref, command_id, is_tab_command, ID_TAB_COLOR_BASE, ID_TAB_COLOR_END};
    use codepp_shell::{CloseMultiKind, TabColor, TabCommand};

    #[test]
    fn colorref_swaps_red_and_blue() {
        assert_eq!(colorref(0x0012_3456), 0x0056_3412);
    }

    /// The bar the tab strip paints for a tab with no color is the
    /// shell's default, which the other backends paint too.
    #[test]
    fn the_default_bar_is_the_shells() {
        assert_eq!(
            crate::TAB_ACTIVE_INDICATOR,
            colorref(codepp_shell::DEFAULT_TAB_BAR_RGB)
        );
    }

    #[test]
    fn colors_map_onto_their_five_ids() {
        let ids: Vec<u16> = TabColor::ALL
            .iter()
            .map(|&c| command_id(TabCommand::ApplyColor(c)))
            .collect();
        assert_eq!(
            ids,
            (ID_TAB_COLOR_BASE..=ID_TAB_COLOR_END).collect::<Vec<_>>()
        );
    }

    #[test]
    fn file_menu_commands_are_not_taken_for_tab_commands() {
        for c in [
            TabCommand::Close,
            TabCommand::Save,
            TabCommand::Print,
            TabCommand::CloseMultiple(CloseMultiKind::AllToLeft),
        ] {
            assert!(!is_tab_command(command_id(c)), "{c:?}");
        }
        for c in [
            TabCommand::TogglePin,
            TabCommand::CopyDirPath,
            TabCommand::MoveToEnd,
            TabCommand::OpenInNewInstance,
            TabCommand::RemoveColor,
        ] {
            assert!(is_tab_command(command_id(c)), "{c:?}");
        }
    }

    /// The production half of a source file with all whitespace removed,
    /// so the guards below match constructs whatever rustfmt does to
    /// their layout.
    fn squashed(src: &str) -> String {
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        prod.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// The menu is shown only for the tab the right click brought to the
    /// front: the activation comes first, and a tab that is not then the
    /// active one gets no menu, so no entry can act on another tab.
    #[test]
    fn the_menu_is_shown_only_after_its_tab_is_activated() {
        let src = squashed(include_str!("tab_menu.rs"));
        let activate = src
            .find("unsafe{handle_window_menu_click(main_hwnd,idx)};")
            .expect("show activates the tab");
        let refuse = src
            .find("ifstate.shell.active_tab!=Some(idx){return;}")
            .expect("show refuses a tab that is not active");
        let build = src
            .find("state.shell.tab_context_menu(idx,dirty)")
            .expect("show builds the shell's menu");
        assert!(activate < refuse && refuse < build);
    }

    /// The chosen entry is posted, so it runs once the popup's modal loop
    /// has returned, and the menu and its swatch bitmaps are freed first.
    #[test]
    fn the_choice_is_posted_after_the_menu_is_freed() {
        let src = squashed(include_str!("tab_menu.rs"));
        let freeze = src
            .find("letfreeze=DrainFreeze::new(main_hwnd);letcmd=unsafe{TrackPopupMenu(")
            .expect("the popup's loop runs under a freeze");
        let track = src.find("TrackPopupMenu(").expect("a popup");
        assert!(freeze < track);
        let checked = src
            .find("ifcmd.0!=0&&unsafe{active_tab_is(main_hwnd,tab_id)}{")
            .expect("the choice is posted only for the tab the menu was for");
        let dropped = src.find("drop(freeze);").expect("the freeze is dropped");
        let destroy = src.find("let_=DestroyMenu(menu);").expect("the menu freed");
        let post = src
            .find("PostMessageW(Some(main_hwnd),WM_COMMAND")
            .expect("posted");
        assert!(track < destroy && destroy < checked && checked < post);
        assert!(
            post < dropped,
            "posted before the freeze re-posts a held wake"
        );
    }

    /// The tab strip's right click goes to the menu only on a tab, and the
    /// main window hands the menu's own commands to this module before
    /// its range checks.
    #[test]
    fn the_strip_and_the_main_window_are_wired() {
        let lib = squashed(include_str!("lib.rs"));
        // The arm in the tab control's subclass procedure, not another
        // window's: from its `fn` to the next `extern "system" fn`.
        let proc_start = lib
            .find("externfntab_subclass_proc(")
            .or_else(|| lib.find("extern\"system\"fntab_subclass_proc("))
            .expect("the tab subclass procedure");
        let proc_end = lib[proc_start + 1..]
            .find("extern\"system\"fn")
            .map_or(lib.len(), |e| proc_start + 1 + e);
        let proc = &lib[proc_start..proc_end];
        let arm = proc.find("WM_RBUTTONUP=>{").expect("a right-click arm");
        let arm_end = proc[arm..]
            .find("WM_LBUTTONUP=>{")
            .map_or(proc.len(), |e| arm + e);
        let body = &proc[arm..arm_end];
        let hit = body
            .find("letSome(idx)=tab_hit_test(hwnd,x,y)else{returnDefSubclassProc(")
            .expect("off a tab, the control's default");
        let show = body
            .find("tab_menu::show(parent,idx,pt);")
            .expect("the menu");
        assert!(hit < show);
        let dispatch = lib
            .find("iftab_menu::is_tab_command(cmd_u16){tab_menu::run_command(hwnd,cmd_u16);returnLRESULT(0);}")
            .expect("the main window hands the menu's commands over");
        let ranges = lib
            .find("if(ID_LANGUAGE_BASE..=ID_LANGUAGE_END).contains(&cmd_u16)")
            .expect("the range checks");
        assert!(
            dispatch < ranges,
            "the menu's commands come before the range checks"
        );
    }

    /// A new instance is started only for a file with nothing unsaved, and
    /// Move closes the tab only once the instance has started.
    #[test]
    fn a_move_closes_the_tab_only_after_the_new_instance_started() {
        let src = squashed(include_str!("tab_menu.rs"));
        let path = src
            .find("state.shell.new_instance_path(idx,dirty)")
            .expect("the shell decides which tab can go");
        let spawn = src
            .find("ifletErr(err)=codepp_shell::spawn_new_instance(&path){")
            .expect("the spawn's failure is handled");
        let bail = src[spawn..].find("return;}").expect("and returns") + spawn;
        let close = src
            .find("ifclose_here&&unsafe{active_tab_is(hwnd,id)}{unsafe{handle_close_active_tab(hwnd);")
            .expect("a move closes the tab it moved, and only that tab");
        assert!(path < spawn && spawn < bail && bail < close);
    }
}
