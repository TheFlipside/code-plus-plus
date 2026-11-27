//! The tab strip's context menu, shared by the three backends.
//!
//! A right click on a document tab first brings that tab to the front, as
//! Notepad++ does, so every entry acts on the active tab and most of them
//! are commands the File menu already has. What this module owns is what
//! the backends must not drift on: the menu's shape, its labels, which
//! entries are greyed for which tab ([`Shell::tab_context_menu`]), and the
//! few operations no menu had before: a tab's color, moving a tab to the
//! start or end of the strip, the text the Copy to Clipboard entries copy,
//! and starting a guest instance of Code++ for a file.
//!
//! Each backend renders the [`TabMenuItem`] tree with its own menu
//! primitive and maps each [`TabCommand`] to its own handler.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{close_multi_enabled, slot_behind_pinned_tabs, untitled_name, CloseMultiKind, Shell};
use codepp_plugin_host::Notification;

/// One of the five colors a tab can be given from its context menu. The
/// number is the one the menu and `session.xml` (`@color`) use.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TabColor {
    /// Color 1.
    Yellow,
    /// Color 2.
    Green,
    /// Color 3.
    Blue,
    /// Color 4.
    Orange,
    /// Color 5.
    Pink,
}

/// The bar a tab without a color shows when it is the active tab: Material
/// orange 400, as `0xRRGGBB`. The default every backend paints.
pub const DEFAULT_TAB_BAR_RGB: u32 = 0x00FF_A726;

/// How much of a color goes into the background tint of an inactive tab
/// that has it, out of 100; the rest is white.
const TINT_PERCENT: u32 = 22;

impl TabColor {
    /// Every color, in menu order.
    pub const ALL: [TabColor; 5] = [
        TabColor::Yellow,
        TabColor::Green,
        TabColor::Blue,
        TabColor::Orange,
        TabColor::Pink,
    ];

    /// The color's number, 1 to 5.
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            TabColor::Yellow => 1,
            TabColor::Green => 2,
            TabColor::Blue => 3,
            TabColor::Orange => 4,
            TabColor::Pink => 5,
        }
    }

    /// The color numbered `n`, or `None` for a number no color has, as a
    /// hand-edited `session.xml` can hold.
    #[must_use]
    pub fn from_number(n: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.number() == n)
    }

    /// The color itself, as `0xRRGGBB`: the bar of the active tab, and the
    /// square beside its menu entry.
    #[must_use]
    pub fn bar_rgb(self) -> u32 {
        match self {
            TabColor::Yellow => 0x00F2_C500,
            TabColor::Green => 0x007C_C94A,
            TabColor::Blue => 0x0042_8FE8,
            TabColor::Orange => 0x00F5_7C00,
            TabColor::Pink => 0x00EC_6A9C,
        }
    }

    /// The light tint an inactive tab with this color takes as its
    /// background, as `0xRRGGBB`.
    #[must_use]
    pub fn tint_rgb(self) -> u32 {
        tint_of(self.bar_rgb())
    }

    /// The tint an inactive tab with this color takes over the background
    /// `base` (`0xRRGGBB`): the same share of the color as
    /// [`Self::tint_rgb`], laid over `base` rather than white. A backend
    /// in the dark scheme passes its tab strip's own background, so the
    /// tint is a dark one; over white it is [`Self::tint_rgb`].
    #[must_use]
    pub fn tint_over(self, base: u32) -> u32 {
        codepp_core::colour::mix(self.bar_rgb(), base, TINT_PERCENT)
    }

    /// The label of the entry that applies it: "Apply Color 1" and so on.
    #[must_use]
    pub fn menu_label(self) -> &'static str {
        match self {
            TabColor::Yellow => "Apply Color 1",
            TabColor::Green => "Apply Color 2",
            TabColor::Blue => "Apply Color 3",
            TabColor::Orange => "Apply Color 4",
            TabColor::Pink => "Apply Color 5",
        }
    }
}

/// `rgb` mixed with white, [`TINT_PERCENT`] of it.
fn tint_of(rgb: u32) -> u32 {
    let mix = |shift: u32| {
        let c = (rgb >> shift) & 0xFF;
        (c * TINT_PERCENT + 0xFF * (100 - TINT_PERCENT)) / 100
    };
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// The bar color of a tab: its own color, or [`DEFAULT_TAB_BAR_RGB`].
#[must_use]
pub fn tab_bar_rgb(color: Option<TabColor>) -> u32 {
    color.map_or(DEFAULT_TAB_BAR_RGB, TabColor::bar_rgb)
}

/// What an entry of the tab context menu does.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TabCommand {
    /// Close the tab.
    Close,
    /// One of the Close Multiple Tabs entries.
    CloseMultiple(CloseMultiKind),
    /// Pin the tab, or unpin it.
    TogglePin,
    /// File → Save.
    Save,
    /// File → Save As.
    SaveAs,
    /// Show the file in the platform's file manager.
    OpenContainingFileManager,
    /// Open a terminal (`cmd` on Windows) in the file's folder.
    OpenContainingTerminal,
    /// Open PowerShell in the file's folder. Windows only.
    OpenContainingPowerShell,
    /// Open the file's folder as the workspace.
    OpenContainingWorkspace,
    /// Hand the file to its default application.
    OpenInDefaultViewer,
    /// File → Rename for a file; the Rename Current Tab dialog for an
    /// untitled buffer.
    Rename,
    /// Move the file to the Recycle Bin and close the tab.
    MoveToRecycleBin,
    /// Reload the file from disk.
    Reload,
    /// File → Print.
    Print,
    /// Copy the file's full path.
    CopyFullPath,
    /// Copy the tab's file name, or an untitled buffer's name.
    CopyFileName,
    /// Copy the path of the file's folder.
    CopyDirPath,
    /// Move the tab to the start of the strip, or of the pinned tabs.
    MoveToStart,
    /// Move the tab to the end of the strip, or of the pinned tabs.
    MoveToEnd,
    /// Not implemented yet: always greyed.
    MoveToOtherView,
    /// Not implemented yet: always greyed.
    CloneToOtherView,
    /// Open the file in a new guest instance and close it here.
    MoveToNewInstance,
    /// Open the file in a new guest instance as well.
    OpenInNewInstance,
    /// Give the tab a color.
    ApplyColor(TabColor),
    /// Take the tab's color away.
    RemoveColor,
}

/// An entry of the tab context menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabMenuItem {
    /// A command entry.
    Command {
        /// The entry's text.
        label: &'static str,
        /// What it does.
        command: TabCommand,
        /// Whether it can be chosen.
        enabled: bool,
        /// The color whose square is drawn left of the text.
        swatch: Option<TabColor>,
    },
    /// An entry that opens a submenu.
    Submenu {
        /// The entry's text.
        label: &'static str,
        /// Whether it can be opened.
        enabled: bool,
        /// The submenu's entries.
        items: Vec<TabMenuItem>,
    },
    /// A separator line.
    Separator,
}

fn cmd(label: &'static str, command: TabCommand, enabled: bool) -> TabMenuItem {
    TabMenuItem::Command {
        label,
        command,
        enabled,
        swatch: None,
    }
}

/// The label of the entry that shows the file in the file manager.
const FILE_MANAGER_LABEL: &str = if cfg!(target_os = "windows") {
    "Open Containing Folder in Explorer"
} else if cfg!(target_os = "macos") {
    "Open Containing Folder in Finder"
} else {
    "Open Containing Folder in File Manager"
};

/// The label of the entry that opens a terminal in the file's folder.
const TERMINAL_LABEL: &str = if cfg!(target_os = "windows") {
    "Open Containing Folder in cmd"
} else {
    "Open Containing Folder in Terminal"
};

/// The title of the dialog that renames an untitled buffer's tab.
pub const RENAME_TAB_TITLE: &str = "Rename Current Tab";

/// The caption of the frame around the new name in that dialog.
pub const RENAME_TAB_FIELD_LABEL: &str = "New name";

/// Which part of a tab's file the Copy to Clipboard entries copy.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClipboardPart {
    /// The full path.
    FullPath,
    /// The file name, or an untitled buffer's name.
    FileName,
    /// The folder's path.
    DirPath,
}

/// Set once, before the UI starts, when this process is a guest instance.
static GUEST_INSTANCE: AtomicBool = AtomicBool::new(false);

/// Make this process a guest instance: it restores no session and saves
/// none, so it writes no `session.xml` and no recovery backups, and on
/// Windows it takes no instance lock. What `--new-instance` asks for, and
/// what [`spawn_new_instance`] starts. Notepad++'s `-multiInst -nosession`.
///
/// A guest shares the configuration folder with the instance it was
/// started from, so it reads the same settings and plugins. Its unsaved
/// edits are protected only by the close prompt: a crash loses them.
pub fn set_guest_instance() {
    GUEST_INSTANCE.store(true, Ordering::Relaxed);
}

/// Whether this process is a guest instance ([`set_guest_instance`]).
#[must_use]
pub fn is_guest_instance() -> bool {
    GUEST_INSTANCE.load(Ordering::Relaxed)
}

/// The command-line flag that makes a process a guest instance.
pub const NEW_INSTANCE_FLAG: &str = "--new-instance";

/// The arguments a guest instance for `path` is started with.
#[must_use]
pub fn new_instance_args(path: &Path) -> Vec<std::ffi::OsString> {
    vec![
        NEW_INSTANCE_FLAG.into(),
        "--".into(),
        path.as_os_str().to_owned(),
    ]
}

/// Start a guest instance of this executable for `path`
/// ([`set_guest_instance`]). It is waited for on a thread of its own, so
/// it leaves no zombie on Unix.
///
/// # Errors
///
/// When the executable's path cannot be had or the process cannot start.
pub fn spawn_new_instance(path: &Path) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe)
        .args(new_instance_args(path))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // The instance has started whatever becomes of this thread: a thread
    // that cannot be made costs a zombie on Unix until Code++ exits, and
    // is logged rather than reported as a failure to start.
    if let Err(err) = std::thread::Builder::new()
        .name("codepp-guest-wait".into())
        .spawn(move || {
            let _ = child.wait();
        })
    {
        tracing::warn!(?err, "a new instance started, but nothing waits for it");
    }
    Ok(())
}

/// Where the tab at `idx` goes for Move to Start (`end` false) or Move to
/// End, as an index in the strip with the tab taken out: the edge of the
/// pinned tabs for a pinned tab, and of the unpinned ones for the others,
/// so the pinned-before-unpinned rule holds.
pub(crate) fn edge_slot(tabs: &[crate::Tab], idx: usize, end: bool) -> usize {
    if tabs[idx].pinned {
        if end {
            // Behind the last pinned tab, counted with this one taken out.
            tabs.iter()
                .enumerate()
                .rev()
                .find(|&(i, t)| i != idx && t.pinned)
                .map_or(0, |(i, _)| if i > idx { i } else { i + 1 })
        } else {
            0
        }
    } else if end {
        tabs.len() - 1
    } else {
        slot_behind_pinned_tabs(tabs, idx)
    }
}

/// The Open into entries, for a tab with a file (`on_disk`) or without.
fn open_into_items(on_disk: bool) -> Vec<TabMenuItem> {
    use TabCommand as C;
    let mut items = vec![
        cmd(FILE_MANAGER_LABEL, C::OpenContainingFileManager, on_disk),
        cmd(TERMINAL_LABEL, C::OpenContainingTerminal, on_disk),
    ];
    if cfg!(target_os = "windows") {
        items.push(cmd(
            "Open Containing Folder in PowerShell",
            C::OpenContainingPowerShell,
            on_disk,
        ));
    }
    items.extend([
        cmd(
            "Open Containing Folder as Workspace",
            C::OpenContainingWorkspace,
            on_disk,
        ),
        TabMenuItem::Separator,
        cmd("Open in Default Viewer", C::OpenInDefaultViewer, on_disk),
    ]);
    items
}

/// The Apply Color to Tab entries: the five colors with their squares,
/// and Remove Color, enabled when the tab has one (`has_color`).
fn color_items(has_color: bool) -> Vec<TabMenuItem> {
    let mut items: Vec<TabMenuItem> = TabColor::ALL
        .into_iter()
        .map(|c| TabMenuItem::Command {
            label: c.menu_label(),
            command: TabCommand::ApplyColor(c),
            enabled: true,
            swatch: Some(c),
        })
        .collect();
    items.push(cmd("Remove Color", TabCommand::RemoveColor, has_color));
    items
}

impl Shell {
    /// The context menu of the tab at `idx`, which the caller has brought
    /// to the front. `dirty` is whether that tab holds unsaved work, as the
    /// backend's close prompt reads it. Empty for an index with no tab.
    #[must_use]
    pub fn tab_context_menu(&self, idx: usize, dirty: bool) -> Vec<TabMenuItem> {
        use TabCommand as C;
        let Some(tab) = self.tabs.get(idx) else {
            return Vec::new();
        };
        let on_disk = tab.path.is_some();
        // A file still on its first load has nothing of its own to save
        // or rename yet, and a save of it is refused (`LoadInFlight`).
        let loading = tab.pending_load.is_some();
        let close_items = self.close_multiple_items();
        let close_any = close_items
            .iter()
            .any(|i| matches!(i, TabMenuItem::Command { enabled: true, .. }));
        let copy = vec![
            cmd("Copy Full File Path", C::CopyFullPath, on_disk),
            cmd("Copy Filename", C::CopyFileName, true),
            cmd("Copy Current Dir. Path", C::CopyDirPath, on_disk),
        ];
        let move_doc = vec![
            cmd(
                "Move to Start",
                C::MoveToStart,
                edge_slot(&self.tabs, idx, false) != idx,
            ),
            cmd(
                "Move to End",
                C::MoveToEnd,
                edge_slot(&self.tabs, idx, true) != idx,
            ),
        ];
        let pin_label = if tab.pinned { "Unpin Tab" } else { "Pin Tab" };
        let new_instance = on_disk && !dirty && !loading;
        vec![
            cmd("Close", C::Close, true),
            TabMenuItem::Submenu {
                label: "Close Multiple Tabs",
                enabled: close_any,
                items: close_items,
            },
            cmd(pin_label, C::TogglePin, true),
            cmd("Save", C::Save, !loading && (!on_disk || dirty)),
            cmd("Save As...", C::SaveAs, !loading),
            TabMenuItem::Submenu {
                label: "Open into",
                enabled: on_disk,
                items: open_into_items(on_disk),
            },
            cmd("Rename...", C::Rename, !loading),
            cmd("Move to Recycle Bin", C::MoveToRecycleBin, on_disk),
            // A file still loading is reloaded by the load itself.
            cmd("Reload", C::Reload, on_disk && !loading),
            cmd("Print...", C::Print, true),
            TabMenuItem::Separator,
            TabMenuItem::Submenu {
                label: "Copy to Clipboard",
                enabled: true,
                items: copy,
            },
            TabMenuItem::Submenu {
                label: "Move Document",
                enabled: true,
                items: move_doc,
            },
            TabMenuItem::Separator,
            cmd("Move to Other View", C::MoveToOtherView, false),
            cmd("Clone to Other View", C::CloneToOtherView, false),
            cmd("Move to New Instance", C::MoveToNewInstance, new_instance),
            cmd("Open in New Instance", C::OpenInNewInstance, new_instance),
            TabMenuItem::Submenu {
                label: "Apply Color to Tab",
                enabled: true,
                items: color_items(tab.color.is_some()),
            },
        ]
    }

    /// The Close Multiple Tabs entries, each enabled when it would close a
    /// tab, as the File menu's are ([`close_multi_enabled`]).
    fn close_multiple_items(&self) -> Vec<TabMenuItem> {
        [
            ("Close All BUT This", CloseMultiKind::AllButActive),
            ("Close All BUT Pinned", CloseMultiKind::AllButPinned),
            ("Close All to the Left", CloseMultiKind::AllToLeft),
            ("Close All to the Right", CloseMultiKind::AllToRight),
            ("Close All Unchanged", CloseMultiKind::AllUnchanged),
        ]
        .into_iter()
        .map(|(label, kind)| {
            cmd(
                label,
                TabCommand::CloseMultiple(kind),
                close_multi_enabled(&self.tabs, self.active_tab, kind),
            )
        })
        .collect()
    }

    /// Give the tab at `idx` `color`, or take its color away with `None`.
    /// Returns `true` on a change. The color is saved with the session,
    /// and a request made while the tab's file is on its first load is
    /// kept over the color the session recorded for that file
    /// ([`crate::Tab::color_requested_during_load`]).
    pub fn set_tab_color(&mut self, idx: usize, color: Option<TabColor>) -> bool {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return false;
        };
        tab.color_requested_during_load = tab.pending_load;
        if tab.color == color {
            return false;
        }
        tab.color = color;
        true
    }

    /// Move the tab at `idx` to the start of the strip (`end` false) or to
    /// its end; a pinned tab moves within the pinned tabs. The active-tab
    /// index follows the tabs, and a real move queues
    /// `NPPN_DOCORDERCHANGED`. Returns `true` on a change.
    pub fn move_tab_to_edge(&mut self, idx: usize, end: bool) -> bool {
        if idx >= self.tabs.len() {
            return false;
        }
        let to = edge_slot(&self.tabs, idx, end);
        if to == idx {
            return false;
        }
        let tab = self.tabs.remove(idx);
        self.tabs.insert(to, tab);
        if let Some(active) = self.active_tab.as_mut() {
            if *active == idx {
                *active = to;
            } else if idx < *active && *active <= to {
                *active -= 1;
            } else if to <= *active && *active < idx {
                *active += 1;
            }
        }
        self.session.active = self.active_tab;
        self.pending_notifications
            .push(Notification::DocOrderChanged);
        true
    }

    /// The text a Copy to Clipboard entry copies for the tab at `idx`, or
    /// `None` when there is nothing to copy: a path or a folder for a tab
    /// with no file. The real name, not the one the tab strip shows with
    /// what would reorder it replaced: the user pastes it somewhere.
    #[must_use]
    pub fn tab_clipboard_text(&self, idx: usize, part: ClipboardPart) -> Option<String> {
        let tab = self.tabs.get(idx)?;
        match (part, tab.path.as_deref()) {
            (ClipboardPart::FullPath, Some(p)) => Some(p.to_string_lossy().into_owned()),
            (ClipboardPart::DirPath, Some(p)) => p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.to_string_lossy().into_owned()),
            (ClipboardPart::FileName, Some(p)) => {
                p.file_name().map(|n| n.to_string_lossy().into_owned())
            }
            (ClipboardPart::FileName, None) => Some(untitled_name(tab)),
            (ClipboardPart::FullPath | ClipboardPart::DirPath, None) => None,
        }
    }

    /// The file of the tab at `idx`, if it has one and nothing unsaved, for
    /// Move to New Instance and Open in New Instance: a guest instance
    /// reads the file from disk.
    #[must_use]
    pub fn new_instance_path(&self, idx: usize, dirty: bool) -> Option<PathBuf> {
        let tab = self.tabs.get(idx)?;
        if dirty || tab.pending_load.is_some() {
            return None;
        }
        tab.path.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        edge_slot, new_instance_args, tint_of, ClipboardPart, TabColor, TabCommand, TabMenuItem,
    };
    use crate::{Shell, Tab};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn shell_with(tabs: Vec<Tab>, active: usize) -> Shell {
        let mut shell = Shell::new(Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>).unwrap();
        shell.tabs = tabs;
        shell.active_tab = Some(active);
        shell
    }

    fn file_tab(id: i32, name: &str) -> Tab {
        Tab {
            id,
            path: Some(PathBuf::from("/some/dir").join(name)),
            ..Tab::default()
        }
    }

    /// The enabled state of the command entry `command` in `items`,
    /// searched through submenus.
    fn find(items: &[TabMenuItem], command: TabCommand) -> Option<bool> {
        items.iter().find_map(|i| match i {
            TabMenuItem::Command {
                command: c,
                enabled,
                ..
            } if *c == command => Some(*enabled),
            TabMenuItem::Submenu { items, .. } => find(items, command),
            _ => None,
        })
    }

    fn tab(id: i32, pinned: bool) -> Tab {
        Tab {
            id,
            pinned,
            ..Tab::default()
        }
    }

    #[test]
    fn a_menu_for_no_tab_is_empty() {
        let shell = shell_with(vec![tab(1, false)], 0);
        assert!(shell.tab_context_menu(1, false).is_empty());
    }

    #[test]
    fn a_relative_file_with_no_folder_copies_no_folder() {
        let t = Tab {
            id: 1,
            path: Some(PathBuf::from("bare.txt")),
            ..Tab::default()
        };
        let shell = shell_with(vec![t], 0);
        assert_eq!(shell.tab_clipboard_text(0, ClipboardPart::DirPath), None);
    }

    #[test]
    fn colors_round_trip_by_number_and_others_are_refused() {
        for c in TabColor::ALL {
            assert_eq!(TabColor::from_number(c.number()), Some(c));
        }
        for n in [0, 6, 255] {
            assert_eq!(TabColor::from_number(n), None);
        }
        let numbers: Vec<u8> = TabColor::ALL.iter().map(|c| c.number()).collect();
        assert_eq!(numbers, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_tint_over_white_is_the_light_tint_and_over_dark_is_dark() {
        for c in TabColor::ALL {
            assert_eq!(c.tint_over(0x00FF_FFFF), c.tint_rgb(), "{c:?}");
            let dark = c.tint_over(0x0020_2020);
            assert!(codepp_core::colour::is_dark(dark), "{c:?}: {dark:06X}");
            // Each channel is the color's share laid over the base's.
            for shift in [16, 8, 0] {
                let bar = (c.bar_rgb() >> shift) & 0xFF;
                let want = (bar * super::TINT_PERCENT + 0x20 * (100 - super::TINT_PERCENT)) / 100;
                assert_eq!((dark >> shift) & 0xFF, want, "{c:?}, shift {shift}");
            }
        }
    }

    #[test]
    fn a_tint_is_lighter_than_its_color() {
        assert_eq!(tint_of(0x00FF_FFFF), 0x00FF_FFFF);
        assert_eq!(tint_of(0), 0x00C6_C6C6);
        for c in TabColor::ALL {
            let (bar, tint) = (c.bar_rgb(), c.tint_rgb());
            for shift in [16, 8, 0] {
                assert!((tint >> shift) & 0xFF >= (bar >> shift) & 0xFF);
            }
        }
    }

    #[test]
    fn edge_slots_keep_pinned_tabs_in_front() {
        // P0 P1 U2 U3 U4
        let tabs = vec![
            tab(1, true),
            tab(2, true),
            tab(3, false),
            tab(4, false),
            tab(5, false),
        ];
        assert_eq!(edge_slot(&tabs, 3, false), 2, "start of the unpinned tabs");
        assert_eq!(edge_slot(&tabs, 3, true), 4);
        assert_eq!(edge_slot(&tabs, 2, false), 2, "already there");
        assert_eq!(edge_slot(&tabs, 1, false), 0, "start of the pinned tabs");
        assert_eq!(edge_slot(&tabs, 0, true), 1, "end of the pinned tabs");
        assert_eq!(edge_slot(&tabs, 1, true), 1, "already there");
    }

    #[test]
    fn new_instance_args_end_flags_before_the_path() {
        let args = new_instance_args(std::path::Path::new("-x.txt"));
        assert_eq!(args, ["--new-instance", "--", "-x.txt"]);
    }

    /// The labels and the order the menu was asked for.
    #[test]
    fn the_menu_has_the_entries_in_order() {
        let shell_menu = |items: &[TabMenuItem]| -> Vec<String> {
            items
                .iter()
                .map(|i| match i {
                    TabMenuItem::Command { label, .. } | TabMenuItem::Submenu { label, .. } => {
                        (*label).to_string()
                    }
                    TabMenuItem::Separator => "-".to_string(),
                })
                .collect()
        };
        let shell = shell_with(vec![tab(1, false)], 0);
        let menu = shell.tab_context_menu(0, false);
        assert_eq!(
            shell_menu(&menu),
            [
                "Close",
                "Close Multiple Tabs",
                "Pin Tab",
                "Save",
                "Save As...",
                "Open into",
                "Rename...",
                "Move to Recycle Bin",
                "Reload",
                "Print...",
                "-",
                "Copy to Clipboard",
                "Move Document",
                "-",
                "Move to Other View",
                "Clone to Other View",
                "Move to New Instance",
                "Open in New Instance",
                "Apply Color to Tab",
            ]
        );
        let TabMenuItem::Submenu { items, .. } = &menu[18] else {
            panic!("no color submenu");
        };
        assert_eq!(items.len(), 6);
        assert!(matches!(
            items[5],
            TabMenuItem::Command {
                command: TabCommand::RemoveColor,
                enabled: false,
                ..
            }
        ));
    }

    #[test]
    fn an_untitled_buffer_greys_what_needs_a_file() {
        let shell = shell_with(vec![tab(1, false)], 0);
        let menu = shell.tab_context_menu(0, false);
        for c in [
            TabCommand::OpenContainingWorkspace,
            TabCommand::OpenInDefaultViewer,
            TabCommand::MoveToRecycleBin,
            TabCommand::Reload,
            TabCommand::CopyFullPath,
            TabCommand::CopyDirPath,
            TabCommand::MoveToNewInstance,
            TabCommand::OpenInNewInstance,
            TabCommand::MoveToOtherView,
            TabCommand::CloneToOtherView,
        ] {
            assert_eq!(find(&menu, c), Some(false), "{c:?}");
        }
        for c in [
            TabCommand::Close,
            TabCommand::Save,
            TabCommand::SaveAs,
            TabCommand::Rename,
            TabCommand::Print,
            TabCommand::CopyFileName,
        ] {
            assert_eq!(find(&menu, c), Some(true), "{c:?}");
        }
        let TabMenuItem::Submenu { enabled, .. } = &menu[5] else {
            panic!("Open into is a submenu");
        };
        assert!(!enabled, "Open into is only for files on disk");
    }

    #[test]
    fn save_is_greyed_for_a_clean_file_and_new_instances_for_a_dirty_one() {
        let shell = shell_with(vec![file_tab(1, "a.txt")], 0);
        let clean = shell.tab_context_menu(0, false);
        assert_eq!(find(&clean, TabCommand::Save), Some(false));
        assert_eq!(find(&clean, TabCommand::OpenInNewInstance), Some(true));
        assert_eq!(find(&clean, TabCommand::Reload), Some(true));
        let dirty = shell.tab_context_menu(0, true);
        assert_eq!(find(&dirty, TabCommand::Save), Some(true));
        assert_eq!(find(&dirty, TabCommand::OpenInNewInstance), Some(false));
        assert_eq!(find(&dirty, TabCommand::MoveToNewInstance), Some(false));
        assert_eq!(shell.new_instance_path(0, true), None);
        assert_eq!(
            shell.new_instance_path(0, false),
            Some(PathBuf::from("/some/dir/a.txt"))
        );
    }

    #[test]
    fn a_file_still_loading_cannot_be_saved_or_renamed() {
        let mut t = file_tab(1, "a.txt");
        t.pending_load = Some(7);
        let shell = shell_with(vec![t], 0);
        let menu = shell.tab_context_menu(0, true);
        for c in [
            TabCommand::Save,
            TabCommand::SaveAs,
            TabCommand::Rename,
            TabCommand::Reload,
        ] {
            assert_eq!(find(&menu, c), Some(false), "{c:?}");
        }
        assert_eq!(shell.new_instance_path(0, false), None);
    }

    #[test]
    fn the_pin_entry_says_what_it_will_do() {
        let shell = shell_with(vec![tab(1, true), tab(2, false)], 0);
        let label = |idx| {
            shell
                .tab_context_menu(idx, false)
                .into_iter()
                .find_map(|i| match i {
                    TabMenuItem::Command {
                        label,
                        command: TabCommand::TogglePin,
                        ..
                    } => Some(label),
                    _ => None,
                })
        };
        assert_eq!(label(0), Some("Unpin Tab"));
        assert_eq!(label(1), Some("Pin Tab"));
    }

    #[test]
    fn clipboard_text_is_the_real_name() {
        let mut untitled = tab(2, false);
        untitled.untitled_seq = Some(4);
        let mut renamed = tab(3, false);
        renamed.custom_name = Some("notes\u{202E}x".to_string());
        let shell = shell_with(vec![file_tab(1, "a.txt"), untitled, renamed], 0);
        let dir = PathBuf::from("/some/dir");
        assert_eq!(
            shell.tab_clipboard_text(0, ClipboardPart::FullPath),
            Some(dir.join("a.txt").to_string_lossy().into_owned())
        );
        assert_eq!(
            shell.tab_clipboard_text(0, ClipboardPart::DirPath),
            Some(dir.to_string_lossy().into_owned())
        );
        assert_eq!(
            shell.tab_clipboard_text(0, ClipboardPart::FileName),
            Some("a.txt".to_string())
        );
        assert_eq!(
            shell.tab_clipboard_text(1, ClipboardPart::FileName),
            Some("new 4".to_string())
        );
        assert_eq!(
            shell.tab_clipboard_text(2, ClipboardPart::FileName),
            Some("notes\u{202E}x".to_string())
        );
        assert_eq!(shell.tab_clipboard_text(1, ClipboardPart::FullPath), None);
        assert_eq!(shell.tab_clipboard_text(1, ClipboardPart::DirPath), None);
    }

    #[test]
    fn moving_to_an_edge_keeps_the_active_tab_and_announces_it() {
        let ids = |s: &Shell| s.tabs.iter().map(|t| t.id).collect::<Vec<_>>();
        let mut shell = shell_with(
            vec![tab(1, true), tab(2, false), tab(3, false), tab(4, false)],
            2,
        );
        assert!(shell.move_tab_to_edge(3, false));
        assert_eq!(ids(&shell), [1, 4, 2, 3], "behind the pinned tab");
        assert_eq!(shell.active_tab, Some(3), "tab 3 is still in front");
        assert!(shell
            .pending_notifications
            .iter()
            .any(|n| matches!(n, codepp_plugin_host::Notification::DocOrderChanged)));
        assert!(shell.move_tab_to_edge(1, true));
        assert_eq!(ids(&shell), [1, 2, 3, 4]);
        assert_eq!(shell.active_tab, Some(2));
        assert!(!shell.move_tab_to_edge(3, true), "already at the end");
        assert!(!shell.move_tab_to_edge(9, true), "no such tab");
        assert_eq!(
            shell.session.active, shell.active_tab,
            "the session follows"
        );
        let mut shell = shell_with(vec![tab(1, true), tab(2, true), tab(3, false)], 0);
        assert!(shell.move_tab_to_edge(0, true));
        assert_eq!(ids(&shell), [2, 1, 3], "a pinned tab stays pinned in front");
        assert_eq!(shell.active_tab, Some(1));
    }

    #[test]
    fn a_color_is_set_taken_away_and_recorded_against_a_load() {
        let mut loading = tab(1, false);
        loading.pending_load = Some(5);
        let mut shell = shell_with(vec![loading], 0);
        assert!(shell.set_tab_color(0, Some(TabColor::Blue)));
        assert_eq!(shell.tabs[0].color, Some(TabColor::Blue));
        assert_eq!(shell.tabs[0].color_requested_during_load, Some(5));
        assert!(!shell.set_tab_color(0, Some(TabColor::Blue)));
        let menu = shell.tab_context_menu(0, false);
        assert_eq!(find(&menu, TabCommand::RemoveColor), Some(true));
        assert!(shell.set_tab_color(0, None));
        assert_eq!(shell.tabs[0].color, None);
        assert!(!shell.set_tab_color(3, None));
    }
}
