//! The tab strip's context menu on macOS.
//!
//! A right click — or a Control-click — on a tab brings that tab to the
//! front through the strip's own path ([`crate::select_tab_by_id`]), as
//! Notepad++ does, and then pops up the menu `codepp_shell` builds for it
//! ([`codepp_shell::Shell::tab_context_menu`]), so the three backends
//! agree on the entries, their labels and which are greyed.
//!
//! **The choice is recorded inside the menu and carried out after it.**
//! `popUpContextMenu:withEvent:forView:` runs a nested run loop and sends
//! the chosen item's action from inside it, while the tab's own
//! `rightMouseDown:` is still on the stack and a [`crate::DrainFreeze`]
//! is up. Most entries rebuild the tab strip (removing that button), and
//! several run plugin code or a modal of their own. So the item's action
//! only stores the command ([`choose`]); [`show`] runs it once the menu
//! has returned and the freeze is down — the decide-inside, act-outside
//! rule the strip's drag loop keeps for the same reason.
//!
//! Every command acts on the tab in front, and is not run at all if the
//! tab the menu was opened for has left the front meanwhile.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use codepp_shell::{ClipboardPart, CloseMultiKind, TabColor, TabCommand, TabMenuItem};
use objc2::rc::Retained;
use objc2::{sel, AnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertStyle, NSBitmapImageRep, NSBox, NSEvent, NSImage, NSImageRep, NSMenu,
    NSMenuItem, NSPasteboard, NSPasteboardTypeString, NSTextAlignment, NSTextField,
    NSTitlePosition, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSData, NSFileManager, NSPoint, NSRect, NSSize, NSString, NSURL,
};

use crate::menu::Actions;
use crate::state::with_state;

/// The tag an entry for `command` carries.
///
/// An exhaustive `match` rather than a table looked up at run time, so a
/// command the shell adds is a compile error here instead of an entry
/// whose choice is silently dropped. [`command_of`] is its inverse; a
/// test checks the two against each other over every tag.
fn tag_of(command: TabCommand) -> isize {
    use TabCommand as C;
    match command {
        C::Close => 0,
        C::CloseMultiple(kind) => match kind {
            CloseMultiKind::AllButActive => 1,
            CloseMultiKind::AllButPinned => 2,
            CloseMultiKind::AllToLeft => 3,
            CloseMultiKind::AllToRight => 4,
            CloseMultiKind::AllUnchanged => 5,
        },
        C::TogglePin => 6,
        C::Save => 7,
        C::SaveAs => 8,
        C::OpenContainingFileManager => 9,
        C::OpenContainingTerminal => 10,
        C::OpenContainingPowerShell => 11,
        C::OpenContainingWorkspace => 12,
        C::OpenInDefaultViewer => 13,
        C::Rename => 14,
        C::MoveToRecycleBin => 15,
        C::Reload => 16,
        C::Print => 17,
        C::CopyFullPath => 18,
        C::CopyFileName => 19,
        C::CopyDirPath => 20,
        C::MoveToStart => 21,
        C::MoveToEnd => 22,
        C::MoveToOtherView => 23,
        C::CloneToOtherView => 24,
        C::MoveToNewInstance => 25,
        C::OpenInNewInstance => 26,
        C::ApplyColor(color) => match color {
            TabColor::Yellow => 27,
            TabColor::Green => 28,
            TabColor::Blue => 29,
            TabColor::Orange => 30,
            TabColor::Pink => 31,
        },
        C::RemoveColor => 32,
    }
}

/// The command an entry's `tag` names, or `None` for a tag no entry has:
/// the inverse of [`tag_of`].
fn command_of(tag: isize) -> Option<TabCommand> {
    use TabCommand as C;
    Some(match tag {
        0 => C::Close,
        1 => C::CloseMultiple(CloseMultiKind::AllButActive),
        2 => C::CloseMultiple(CloseMultiKind::AllButPinned),
        3 => C::CloseMultiple(CloseMultiKind::AllToLeft),
        4 => C::CloseMultiple(CloseMultiKind::AllToRight),
        5 => C::CloseMultiple(CloseMultiKind::AllUnchanged),
        6 => C::TogglePin,
        7 => C::Save,
        8 => C::SaveAs,
        9 => C::OpenContainingFileManager,
        10 => C::OpenContainingTerminal,
        11 => C::OpenContainingPowerShell,
        12 => C::OpenContainingWorkspace,
        13 => C::OpenInDefaultViewer,
        14 => C::Rename,
        15 => C::MoveToRecycleBin,
        16 => C::Reload,
        17 => C::Print,
        18 => C::CopyFullPath,
        19 => C::CopyFileName,
        20 => C::CopyDirPath,
        21 => C::MoveToStart,
        22 => C::MoveToEnd,
        23 => C::MoveToOtherView,
        24 => C::CloneToOtherView,
        25 => C::MoveToNewInstance,
        26 => C::OpenInNewInstance,
        27 => C::ApplyColor(TabColor::Yellow),
        28 => C::ApplyColor(TabColor::Green),
        29 => C::ApplyColor(TabColor::Blue),
        30 => C::ApplyColor(TabColor::Orange),
        31 => C::ApplyColor(TabColor::Pink),
        32 => C::RemoveColor,
        _ => return None,
    })
}

thread_local! {
    /// The entry chosen in the menu now up, written by [`choose`] from
    /// inside the menu's run loop and taken by [`show`] after it.
    static CHOSEN: Cell<Option<TabCommand>> = const { Cell::new(None) };
}

/// Record the entry with `tag` as the menu's choice. The action of every
/// entry; see the module docs for why it carries nothing out itself.
pub(crate) fn choose(tag: isize) {
    CHOSEN.with(|c| c.set(command_of(tag)));
}

/// Bring the tab with buffer id `id` to the front and show its context
/// menu for `event`, then carry out the entry chosen.
///
/// The caller is the tab's own mouse handler, which holds an owned
/// reference to itself across this call: bringing the tab to the front
/// rebuilds the strip, removing that button. The menu is therefore shown
/// for the strip's container, which stays in the window; the event's
/// location places it.
pub(crate) fn show(id: i32, event: &NSEvent) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if crate::quitting() {
        return;
    }
    crate::select_tab_by_id(id);
    // Deliver the switch before the menu, as a left click does.
    crate::drain_shell();
    if !crate::active_tab_is(id) {
        return;
    }
    let dirty = active_has_unsaved_work();
    let built = with_state(|st| {
        let idx = st.shell.active_tab?;
        Some((
            st.shell.tab_context_menu(idx, dirty),
            st.actions.clone(),
            st.tabs.container.clone(),
        ))
    });
    let Some(Some((items, actions, anchor))) = built else {
        if built.is_none() {
            tracing::debug!(id, "the tab context menu was not shown: the state was busy");
        }
        return;
    };
    let menu = build_menu(&items, &actions, mtm);
    CHOSEN.with(|c| c.set(None));
    // `popUpContextMenu:` spins a nested run loop, and GCD's main-queue
    // source is serviced there, so a worker wake fires during the menu.
    // Every nested loop on this backend takes the freeze.
    let freeze = crate::DrainFreeze::new();
    NSMenu::popUpContextMenu_withEvent_forView(&menu, event, &anchor);
    drop(freeze);
    let chosen = CHOSEN.with(Cell::take);
    if let Some(command) = chosen {
        // The menu's run loop can run plugin code (a timer of its own):
        // act only on the tab the menu was about.
        if crate::active_tab_is(id) {
            run(command, id);
        } else {
            tracing::debug!(id, "the tab left the front while its context menu was up");
        }
    }
    // Replays what the freeze held back, and delivers what the command
    // queued. Unconditional, as the workspace menu's flush is.
    crate::drain_shell();
}

/// Whether the tab in front holds unsaved work, as the close prompt reads
/// it: the live modify bit and the restore marker, and the cached bit,
/// which alone records a file the watcher saw deleted. A state that cannot
/// be read counts as unsaved, which only greys Open in New Instance.
fn active_has_unsaved_work() -> bool {
    let cached = with_state(|st| st.shell.active().is_some_and(|t| t.dirty)).unwrap_or(true);
    cached || crate::active_dirty() != Some(false)
}

/// A menu holding `items`, every entry aimed at `actions`' one tab-menu
/// action and enabled as the shell says.
///
/// Automatic enabling is turned off on these menus — and only on these:
/// the main menus rely on it for their marks (DESIGN.md §7.2, m3d) — so
/// that the greying is the shell's, not a guess by `validateMenuItem:`.
fn build_menu(items: &[TabMenuItem], actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    for item in items {
        match item {
            TabMenuItem::Separator => menu.addItem(&NSMenuItem::separatorItem(mtm)),
            TabMenuItem::Command {
                label,
                command,
                enabled,
                swatch,
            } => {
                let entry = NSMenuItem::new(mtm);
                entry.setTitle(&NSString::from_str(label));
                entry.setTag(tag_of(*command));
                // SAFETY: `Actions` implements `codeppTabMenuCommand:`, and
                // AppKit holds a menu item's target weakly against an
                // object the window state owns for the process lifetime.
                unsafe {
                    entry.setAction(Some(sel!(codeppTabMenuCommand:)));
                    entry.setTarget(Some(actions));
                }
                entry.setEnabled(*enabled);
                if let Some(color) = swatch {
                    if let Some(image) = swatch_image(color.bar_rgb()) {
                        entry.setImage(Some(&image));
                    }
                }
                menu.addItem(&entry);
            }
            TabMenuItem::Submenu {
                label,
                enabled,
                items,
            } => {
                let entry = NSMenuItem::new(mtm);
                let title = NSString::from_str(label);
                entry.setTitle(&title);
                let sub = build_menu(items, actions, mtm);
                sub.setTitle(&title);
                entry.setSubmenu(Some(&sub));
                entry.setEnabled(*enabled);
                menu.addItem(&entry);
            }
        }
    }
    menu
}

/// Side of the square beside an Apply Color entry, in points.
const SWATCH_PT: u32 = 12;
/// Pixels per point in the square's bitmap: drawn at Retina density, and
/// scaled down where a display has one pixel per point.
const SWATCH_SCALE: u32 = 2;

/// A [`SWATCH_PT`]-square image of `rgb` (`0xRRGGBB`) with a grey border,
/// or `None` when it will not decode.
///
/// Made from an in-memory BMP, which `NSBitmapImageRep` decodes as it
/// decodes the tab icons' PNGs, so no drawing context is needed. The
/// bitmap has [`SWATCH_SCALE`] pixels per point, and the representation
/// is given its size in points, so it is sharp on a Retina display.
fn swatch_image(rgb: u32) -> Option<Retained<NSImage>> {
    let bytes = swatch_bmp(rgb, SWATCH_PT * SWATCH_SCALE, SWATCH_SCALE);
    let data = NSData::with_bytes(&bytes);
    let rep = NSBitmapImageRep::imageRepWithData(&data)?;
    let rep: Retained<NSImageRep> = Retained::into_super(rep);
    let points = f64::from(SWATCH_PT);
    let size = NSSize::new(points, points);
    // Without this the representation reports one point per pixel, and
    // the 24-pixel bitmap would be drawn 24 points wide.
    rep.setSize(size);
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    image.addRepresentation(&rep);
    Some(image)
}

/// A 24-bit BMP file, `side` pixels square, of `rgb` inside a grey border
/// `border` pixels wide.
fn swatch_bmp(rgb: u32, side: u32, border: u32) -> Vec<u8> {
    let row = (side * 3).div_ceil(4) * 4;
    let pixels = row * side;
    let mut out = Vec::with_capacity(54 + pixels as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54 + pixels).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&side.to_le_bytes());
    out.extend_from_slice(&side.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&pixels.to_le_bytes());
    out.extend_from_slice(&[0; 16]);
    let [_, r, g, b] = rgb.to_be_bytes();
    for y in 0..side {
        for x in 0..side {
            let edge = x < border || y < border || x + border >= side || y + border >= side;
            // BGR order.
            if edge {
                out.extend_from_slice(&[0x60, 0x60, 0x60]);
            } else {
                out.extend_from_slice(&[b, g, r]);
            }
        }
        out.resize(out.len() + (row - side * 3) as usize, 0);
    }
    out
}

/// Carry out `command` on the tab in front, which is the tab with buffer
/// id `id`.
fn run(command: TabCommand, id: i32) {
    match command {
        TabCommand::Close => {
            crate::action_close_tab();
        }
        TabCommand::CloseMultiple(kind) => close_multiple(kind),
        TabCommand::TogglePin => crate::toggle_pin_by_id(id),
        TabCommand::Save => crate::action_save_file(),
        TabCommand::SaveAs => crate::action_save_file_as(),
        TabCommand::OpenContainingFileManager => {
            if let Some(path) = active_path() {
                reveal_in_finder(&path);
            }
        }
        TabCommand::OpenContainingTerminal => {
            if let Some(dir) = active_path().as_deref().and_then(Path::parent) {
                open_terminal_in(dir);
            }
        }
        TabCommand::OpenContainingWorkspace => {
            if let Some(dir) = active_path().as_deref().and_then(Path::parent) {
                crate::workspace::open_at(dir);
            }
        }
        TabCommand::OpenInDefaultViewer => {
            if let Some(path) = active_path() {
                if let Some(url) = file_url(&path) {
                    NSWorkspace::sharedWorkspace().openURL(&url);
                }
            }
        }
        TabCommand::Rename => rename_active(id),
        TabCommand::MoveToRecycleBin => move_to_trash(id),
        TabCommand::Reload => crate::action_reload(),
        TabCommand::Print => crate::print::show(),
        TabCommand::CopyFullPath => copy_part(ClipboardPart::FullPath),
        TabCommand::CopyFileName => copy_part(ClipboardPart::FileName),
        TabCommand::CopyDirPath => copy_part(ClipboardPart::DirPath),
        TabCommand::MoveToStart | TabCommand::MoveToEnd => {
            let end = command == TabCommand::MoveToEnd;
            with_state(|st| {
                if let Some(idx) = st.shell.active_tab {
                    st.shell.move_tab_to_edge(idx, end);
                }
            });
            crate::refresh_tab_chrome();
        }
        TabCommand::MoveToNewInstance | TabCommand::OpenInNewInstance => {
            new_instance(id, command == TabCommand::MoveToNewInstance);
        }
        TabCommand::ApplyColor(color) => set_color(Some(color)),
        TabCommand::RemoveColor => set_color(None),
        // There is no PowerShell entry on this platform, and the two
        // other-view entries are always greyed.
        TabCommand::OpenContainingPowerShell
        | TabCommand::MoveToOtherView
        | TabCommand::CloneToOtherView => {}
    }
}

/// The file of the tab in front, if it has one.
fn active_path() -> Option<PathBuf> {
    with_state(|st| st.shell.active().and_then(|t| t.path.clone())).flatten()
}

/// A file URL for `path`, made from the path's own bytes, so that a name
/// which is not valid UTF-8 names that file and not one whose name is its
/// lossy decoding. `None` for a path holding a NUL byte, which no file on
/// macOS has.
fn file_url(path: &Path) -> Option<Retained<NSURL>> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let ptr = NonNull::new(bytes.as_ptr().cast_mut())?;
    // SAFETY: `ptr` is a NUL-terminated C string that outlives the call,
    // which copies it; the method only reads it.
    Some(unsafe {
        NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(ptr, false, None)
    })
}

/// Show `path` selected in a Finder window.
fn reveal_in_finder(path: &Path) {
    let Some(url) = file_url(path) else {
        return;
    };
    let urls = NSArray::from_retained_slice(&[url]);
    NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&urls);
}

/// Open Terminal in `dir`: `/usr/bin/open -a Terminal <dir>`, the path
/// one argument of its own, through no shell. Waited for on a thread of
/// its own, so it leaves no zombie.
fn open_terminal_in(dir: &Path) {
    match std::process::Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(dir)
        .spawn()
    {
        Ok(mut child) => {
            // `open` returns at once; the thread only reaps it. If the
            // thread cannot start, the exited `open` stays a zombie until
            // Code++ exits, which costs a process-table entry and nothing
            // else, so it is logged and not reported.
            if let Err(err) = std::thread::Builder::new()
                .name("codepp-open-terminal".into())
                .spawn(move || {
                    let _ = child.wait();
                })
            {
                tracing::warn!(?err, "no thread to reap `open -a Terminal`");
            }
        }
        Err(err) => tracing::warn!(?err, ?dir, "Terminal could not be opened"),
    }
}

/// Copy `part` of the tab in front to the general pasteboard.
fn copy_part(part: ClipboardPart) {
    let text = with_state(|st| {
        let idx = st.shell.active_tab?;
        st.shell.tab_clipboard_text(idx, part)
    })
    .flatten();
    let Some(text) = text else {
        return;
    };
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    // SAFETY: a plain string for a standard pasteboard type.
    unsafe {
        pasteboard.setString_forType(&NSString::from_str(&text), NSPasteboardTypeString);
    }
}

/// Give the tab in front `color`, or take its color away.
fn set_color(color: Option<TabColor>) {
    let changed = with_state(|st| {
        st.shell
            .active_tab
            .is_some_and(|idx| st.shell.set_tab_color(idx, color))
    })
    .unwrap_or(false);
    if changed {
        crate::refresh_tab_chrome();
    }
}

/// Close every tab `kind` names, one at a time through the same path a
/// tab's ✕ takes, so each dirty buffer gets its own Save / Don't Save /
/// Cancel; a close that does not happen ends the run. `ui_gtk`'s
/// `close_multiple_documents`.
fn close_multiple(kind: CloseMultiKind) {
    let Some(Some((keeper, count))) =
        with_state(|st| st.shell.active().map(|t| (t.id, st.shell.tabs.len())))
    else {
        return;
    };
    for _ in 0..count {
        let target = with_state(|st| {
            codepp_shell::pick_next_close_target(&st.shell.tabs, kind, keeper)
                .and_then(|idx| st.shell.tabs.get(idx).map(|t| t.id))
        })
        .flatten();
        let Some(target) = target else {
            break;
        };
        if !crate::close_tab_by_id(target) {
            break;
        }
    }
}

/// Rename: an untitled buffer gets a new display name through the Rename
/// Current Tab prompt; a buffer with a file goes to Save As, as File →
/// Rename does on the other two backends.
fn rename_active(id: i32) {
    let Some(Some((has_path, current))) = with_state(|st| {
        st.shell
            .active()
            .map(|t| (t.path.is_some(), codepp_shell::tab_display_name(t)))
    }) else {
        return;
    };
    if has_path {
        crate::action_save_file_as();
        return;
    }
    let Some(name) = prompt_rename(&current) else {
        return;
    };
    // The prompt's run loop can run plugin code: rename only the tab it
    // was about.
    if !crate::active_tab_is(id) {
        return;
    }
    let changed = with_state(|st| st.shell.set_active_custom_name(&name)).unwrap_or(false);
    if changed {
        crate::refresh_tab_chrome();
    }
}

/// The longest name the prompt keeps, in characters: Win32's dialog's
/// limit.
const RENAME_MAX_CHARS: usize = 255;

/// Ask for a tab's new name, `current` filled in; `None` on Cancel, and
/// for a name of nothing but spaces, which Win32's dialog will not take
/// either.
fn prompt_rename(current: &str) -> Option<String> {
    let mtm = MainThreadMarker::new()?;
    let _freeze = crate::DrainFreeze::new();
    let (alert, field) = build_rename_alert(current, mtm);
    // `NSAlertFirstButtonReturn` is 1000 — OK.
    if alert.runModal() != 1000 {
        return None;
    }
    // Commit what the field editor holds to the field, so a name typed
    // and confirmed with the OK button is the one read.
    field.validateEditing();
    rename_value(&field.stringValue().to_string())
}

/// What the prompt keeps of `typed`: trimmed, at most
/// [`RENAME_MAX_CHARS`] characters, and `None` when nothing is left.
fn rename_value(typed: &str) -> Option<String> {
    let trimmed = typed.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(RENAME_MAX_CHARS).collect())
}

/// The Rename Current Tab prompt: an alert headed
/// [`codepp_shell::RENAME_TAB_TITLE`] whose accessory is a box titled
/// [`codepp_shell::RENAME_TAB_FIELD_LABEL`], its title centred on the top
/// line, around a field holding `current`.
fn build_rename_alert(
    current: &str,
    mtm: MainThreadMarker,
) -> (Retained<NSAlert>, Retained<NSTextField>) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(codepp_shell::RENAME_TAB_TITLE));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    let field = NSTextField::initWithFrame(
        NSTextField::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(240.0, 24.0)),
    );
    // The name as the tab strip shows it, so nothing in it reorders the
    // field.
    field.setStringValue(&NSString::from_str(current));
    let frame = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(264.0, 64.0)),
    );
    frame.setTitle(&NSString::from_str(codepp_shell::RENAME_TAB_FIELD_LABEL));
    frame.setTitlePosition(NSTitlePosition::AtTop);
    // Centre the title. `titleCell` is an `NSCell`; its alignment is the
    // one AppKit offers for a box's title.
    let cell = frame.titleCell();
    // SAFETY: `NSBox`'s title cell is an `NSCell`, which responds to
    // `setAlignment:` taking an `NSTextAlignment`.
    unsafe {
        let _: () = objc2::msg_send![&*cell, setAlignment: NSTextAlignment::Center];
    }
    frame.setContentViewMargins(NSSize::new(10.0, 8.0));
    frame.setContentView(Some(&field));
    frame.sizeToFit();
    alert.setAccessoryView(Some(&frame));
    // Focus the field, so typing replaces the name rather than reaching
    // the buttons.
    alert.window().setInitialFirstResponder(Some(&field));
    (alert, field)
}

/// Move to Recycle Bin: after a confirmation, move the tab's file to the
/// Trash and close the tab — only once the file has moved, so a failure
/// leaves both as they were. `ui_gtk`'s order, for its reason.
fn move_to_trash(id: i32) {
    let Some(path) = active_path() else {
        return;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let confirmed = {
        let _freeze = crate::DrainFreeze::new();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Move to Recycle Bin"));
        alert.setInformativeText(&NSString::from_str(&format!(
            "The file “{}” will be moved to the Trash and this tab will be closed. \
             Unsaved changes to it are lost.",
            codepp_shell::sanitize_path_for_display(&path)
        )));
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.addButtonWithTitle(&NSString::from_str("Move to Trash"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.runModal() == 1000
    };
    // The alert's run loop can run plugin code: act only on the tab the
    // question was about, and on the file it named.
    if !confirmed || !crate::active_tab_is(id) || active_path().as_deref() != Some(path.as_path()) {
        return;
    }
    let Some(url) = file_url(&path) else {
        return;
    };
    let manager = NSFileManager::defaultManager();
    if let Err(err) = manager.trashItemAtURL_resultingItemURL_error(&url, None) {
        crate::error_alert(
            "Move to Recycle Bin failed",
            &codepp_shell::sanitize_str_for_display(&err.localizedDescription().to_string()),
        );
        return;
    }
    // The file and its edits are being discarded with consent, so the
    // close should not ask to save them, which would write the file back.
    // The buffer is marked clean only when the view is known to show this
    // tab's own document: the view can trail the front (DESIGN.md §7.4),
    // and a save point sent to whatever it shows would mark another tab's
    // document clean. Otherwise the close asks, and Don't Save is the
    // answer that keeps the file in the Trash.
    let marked = with_state(|st| {
        let tab = st.shell.active().filter(|t| t.id == id)?;
        let bound = st
            .editor
            .send(codepp_scintilla_sys::SCI_GETDOCPOINTER, 0, 0);
        (tab.scintilla_doc != 0 && bound == tab.scintilla_doc).then(|| {
            st.editor.send(codepp_scintilla_sys::SCI_SETSAVEPOINT, 0, 0);
        })
    });
    if !matches!(marked, Some(Some(()))) {
        tracing::debug!(
            id,
            "the trashed tab's document is not the one in the view; its close will ask"
        );
    }
    crate::refresh_tab_chrome();
    // The refresh delivers nothing, but look again all the same: close
    // only the tab whose file went to the Trash.
    if !crate::active_tab_is(id) || !crate::action_close_tab() {
        tracing::warn!(
            ?path,
            "the tab was not closed after its file went to the Trash; it stays on a deleted path"
        );
    }
}

/// Open the tab's file in a guest instance
/// ([`codepp_shell::spawn_new_instance`]) and, for a move, close it here
/// once the new instance has started.
fn new_instance(id: i32, close_here: bool) {
    // Read now, not when the menu was built: the menu's run loop, or a
    // modal a command before this one ran, can have left the buffer
    // edited since.
    let dirty = active_has_unsaved_work();
    let path = with_state(|st| {
        let idx = st.shell.active_tab?;
        st.shell.new_instance_path(idx, dirty)
    })
    .flatten();
    // The entry is greyed for a tab with no file or unsaved work.
    let Some(path) = path else {
        return;
    };
    if let Err(err) = codepp_shell::spawn_new_instance(&path) {
        tracing::warn!(?err, ?path, "a new instance could not be started");
        crate::error_alert(
            "New Instance",
            &codepp_shell::sanitize_str_for_display(&format!(
                "Code++ could not start a new instance. {err}"
            )),
        );
        return;
    }
    if close_here && crate::active_tab_is(id) {
        crate::action_close_tab();
    }
}

#[cfg(test)]
mod tests {
    use super::{command_of, rename_value, swatch_bmp, tag_of};
    use codepp_shell::{CloseMultiKind, TabColor, TabCommand};

    /// How many tags there are: one per [`TabCommand`], `0..TAG_COUNT`.
    const TAG_COUNT: isize = 33;

    /// Every command there is. The `match` below has no wildcard, so a
    /// command the shell adds fails to compile here until it is listed.
    fn every_command() -> Vec<TabCommand> {
        use TabCommand as C;
        let all = vec![
            C::Close,
            C::CloseMultiple(CloseMultiKind::AllButActive),
            C::CloseMultiple(CloseMultiKind::AllButPinned),
            C::CloseMultiple(CloseMultiKind::AllToLeft),
            C::CloseMultiple(CloseMultiKind::AllToRight),
            C::CloseMultiple(CloseMultiKind::AllUnchanged),
            C::TogglePin,
            C::Save,
            C::SaveAs,
            C::OpenContainingFileManager,
            C::OpenContainingTerminal,
            C::OpenContainingPowerShell,
            C::OpenContainingWorkspace,
            C::OpenInDefaultViewer,
            C::Rename,
            C::MoveToRecycleBin,
            C::Reload,
            C::Print,
            C::CopyFullPath,
            C::CopyFileName,
            C::CopyDirPath,
            C::MoveToStart,
            C::MoveToEnd,
            C::MoveToOtherView,
            C::CloneToOtherView,
            C::MoveToNewInstance,
            C::OpenInNewInstance,
            C::ApplyColor(TabColor::Yellow),
            C::ApplyColor(TabColor::Green),
            C::ApplyColor(TabColor::Blue),
            C::ApplyColor(TabColor::Orange),
            C::ApplyColor(TabColor::Pink),
            C::RemoveColor,
        ];
        for c in &all {
            // The witness: one arm per variant, nested ones included.
            match c {
                C::Close
                | C::TogglePin
                | C::Save
                | C::SaveAs
                | C::OpenContainingFileManager
                | C::OpenContainingTerminal
                | C::OpenContainingPowerShell
                | C::OpenContainingWorkspace
                | C::OpenInDefaultViewer
                | C::Rename
                | C::MoveToRecycleBin
                | C::Reload
                | C::Print
                | C::CopyFullPath
                | C::CopyFileName
                | C::CopyDirPath
                | C::MoveToStart
                | C::MoveToEnd
                | C::MoveToOtherView
                | C::CloneToOtherView
                | C::MoveToNewInstance
                | C::OpenInNewInstance
                | C::RemoveColor
                | C::CloseMultiple(
                    CloseMultiKind::AllButActive
                    | CloseMultiKind::AllButPinned
                    | CloseMultiKind::AllToLeft
                    | CloseMultiKind::AllToRight
                    | CloseMultiKind::AllUnchanged,
                )
                | C::ApplyColor(
                    TabColor::Yellow
                    | TabColor::Green
                    | TabColor::Blue
                    | TabColor::Orange
                    | TabColor::Pink,
                ) => {}
            }
        }
        all
    }

    #[test]
    fn every_command_has_its_own_tag_and_back() {
        let all = every_command();
        assert_eq!(
            isize::try_from(all.len()).unwrap(),
            TAG_COUNT,
            "a command is missing from the list, or TAG_COUNT is stale"
        );
        for c in &all {
            let tag = tag_of(*c);
            assert!((0..TAG_COUNT).contains(&tag), "{c:?} has tag {tag}");
            assert_eq!(command_of(tag), Some(*c), "{c:?}");
        }
        for tag in 0..TAG_COUNT {
            let c = command_of(tag).unwrap_or_else(|| panic!("tag {tag} names nothing"));
            assert_eq!(tag_of(c), tag, "{c:?}");
        }
        for tag in [-1, TAG_COUNT, 99] {
            assert_eq!(command_of(tag), None);
        }
    }

    #[test]
    fn a_swatch_bmp_is_well_formed() {
        // 24 px at 3 bytes is 72, already a multiple of 4.
        let bmp = swatch_bmp(0x0012_3456, 24, 2);
        assert_eq!(&bmp[..2], b"BM");
        assert_eq!(bmp.len(), 54 + 72 * 24);
        let declared = u32::from_le_bytes([bmp[2], bmp[3], bmp[4], bmp[5]]) as usize;
        assert_eq!(declared, bmp.len());
        let pixel = |x: usize, y: usize| {
            let at = 54 + 72 * y + 3 * x;
            [bmp[at], bmp[at + 1], bmp[at + 2]]
        };
        // A two-pixel border, then the color, in BGR order.
        assert_eq!(pixel(1, 5), [0x60, 0x60, 0x60]);
        assert_eq!(pixel(22, 5), [0x60, 0x60, 0x60]);
        assert_eq!(pixel(2, 5), [0x56, 0x34, 0x12]);
        assert_eq!(pixel(21, 21), [0x56, 0x34, 0x12]);
        // A row of an odd side is padded to 4 bytes.
        assert_eq!(swatch_bmp(0, 5, 1).len(), 54 + 16 * 5);
    }

    #[test]
    fn a_rename_keeps_a_trimmed_capped_name() {
        assert_eq!(rename_value("  notes  ").as_deref(), Some("notes"));
        assert_eq!(rename_value("   "), None);
        assert_eq!(
            rename_value("x".repeat(300).as_str()).map(|s| s.len()),
            Some(255)
        );
    }
}
