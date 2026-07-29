//! The menu bar and its handlers.
//!
//! Wired: File (New, Open, Open Containing Folder, Open in Default Viewer,
//! Open Folder as Workspace, Reload from Disk, Save, Save As, Save All,
//! Rename, Close, Close All, Close Multiple Documents, Move to Recycle Bin,
//! Load/Save Session, Print, recent files, Exit), Edit (Undo/Redo,
//! Cut/Copy/Paste/Delete, Select All),
//! Search (Find, Replace, Find Next/Previous, Go to), View (zoom, Word
//! Wrap, Show Whitespace, Show EOL), Encoding (UTF-8 / UTF-8 BOM / UTF-16
//! LE·BE BOM, ANSI greyed), Language (Normal Text + the ~88
//! Lexilla-backed languages, letter-grouped, plus the User-Defined
//! language submenu) and ? (About). Still to come, tracked against the
//! Win32 parity list: Settings, Tools, Macro, Run, Plugins and Window.
//!
//! Accelerators match the Win32 backend's `CreateAcceleratorTableW`
//! block, which DESIGN.md §7.5 names as the source of truth for
//! hotkeys across all three platforms.

use std::path::{Path, PathBuf};

use codepp_shell::{OpenFileOutcome, UiPlatform};
use gtk::gdk::keys::constants as key;
use gtk::glib;
use gtk::glib::prelude::ToVariant;
use gtk::{gio, prelude::*};

use crate::state::with_state;
use crate::{
    close_active_tab, drain_shell, rebind_active_view, refresh_tab_chrome, save_session_now,
    sync_tab_strip,
};

/// Menu item labels paired with the accelerator each one advertises.
/// Kept next to the handler wiring so a label and its shortcut cannot
/// drift apart.
struct Entry {
    label: &'static str,
    /// `None` for an item with no application accelerator — either it has
    /// no shortcut, or the key is left to Scintilla's own keymap (Delete
    /// forward-deletes there; the menu just exposes the command).
    accel: Option<(gtk::gdk::keys::Key, gtk::gdk::ModifierType)>,
    action: fn(),
}

/// The pieces [`build`] hands back: the strip that goes into the window,
/// and the main menu bar addressed by position.
pub struct MenuBarParts {
    /// The horizontal strip packed into the window: the main menu bar
    /// (expanding to fill) followed by the right-shortcut group. The
    /// menu-hide toggle flips *this* so both groups hide together, matching
    /// Win32 where the right shortcuts live on the one menu bar.
    pub row: gtk::Box,
    /// The main menu bar (File … ?). [`connect`] / [`submenu_at`] address it
    /// by position, so it is handed out separately from the strip.
    pub bar: gtk::MenuBar,
}

/// Build the menu bar and its right-edge shortcut group. Menu handlers are
/// attached separately by [`connect`] (they need the window state installed
/// first); the right-shortcut handlers are wired here — see
/// [`build_right_shortcuts`].
pub fn build() -> MenuBarParts {
    let bar = gtk::MenuBar::new();
    // Order mirrors Notepad++/Win32: File, Edit, Search, View, Encoding,
    // Language, Settings, Plugins, ?. "?" is N++'s Help menu; kept as-is
    // for parity. The menus Win32 has that GTK doesn't build yet (Tools,
    // Macro, Run, Window) are omitted, so Plugins sits directly after
    // Settings here — their absence just leaves a gap, not a mis-order.
    for title in [
        "_File",
        "_Edit",
        "_Search",
        "_View",
        "E_ncoding",
        "_Language",
        "Se_ttings",
        "_Plugins",
        "?",
    ] {
        let root = gtk::MenuItem::with_mnemonic(title);
        root.set_submenu(Some(&gtk::Menu::new()));
        bar.append(&root);
    }

    // The main bar expands to fill; the right-shortcut bar is packed to the
    // far end so its three glyphs hug the right edge — the deterministic GTK
    // equivalent of Win32's `MFT_RIGHTJUSTIFY` group (GTK 3's
    // `set_right_justified` only ever right-anchors a single item, so it
    // cannot group three).
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.pack_start(&bar, true, true, 0);
    row.pack_end(&build_right_shortcuts(), false, false, 0);

    MenuBarParts { row, bar }
}

/// The `＋ ▼ X` group pinned to the far right of the menu strip, mirroring
/// Win32's "right shortcuts": new untitled, open-files switcher, close
/// active tab. Rendered as a second `gtk::MenuBar` so the three appear as
/// native menu-bar entries — in this left-to-right order — flush right.
///
/// Handlers are wired here rather than in [`connect`]: they only close over
/// [`with_state`], which resolves the installed state at click time, so
/// `connect`'s "state must be installed first" constraint does not apply.
fn build_right_shortcuts() -> gtk::MenuBar {
    let bar = gtk::MenuBar::new();

    // ＋ new untitled. U+FF0B FULLWIDTH PLUS SIGN, as on Win32, so the glyph
    // sits at the menu font's full em height rather than ASCII '+'s
    // x-height. `with_label` (not `with_mnemonic`) so the glyph is literal.
    let new_item = gtk::MenuItem::with_label("\u{FF0B}");
    new_item.set_tooltip_text(Some("New"));
    new_item.connect_activate(|_| on_new());
    bar.append(&new_item);

    // ▼ open-files switcher. Its submenu is rebuilt from the live tab list
    // on every open (`connect_show`); each row switches to that buffer.
    let list_item = gtk::MenuItem::with_label("\u{25BC}");
    list_item.set_tooltip_text(Some("Switch to open file"));
    let list_menu = gtk::Menu::new();
    list_menu.connect_show(populate_open_files_menu);
    list_item.set_submenu(Some(&list_menu));
    bar.append(&list_item);

    // X close active tab — the same handler as File → Close.
    let close_item = gtk::MenuItem::with_label("X");
    close_item.set_tooltip_text(Some("Close"));
    close_item.connect_activate(|_| on_close());
    bar.append(&close_item);

    bar
}

/// (Re)populate the `▼` open-files switcher from the live tab list. Rebuilt
/// on every open so it always matches `Shell`. Each row switches to its
/// buffer by stable id — robust to reordering, exactly as the tab strip's
/// close buttons are keyed (see the `tabs` module docs). The active buffer's
/// row is drawn checked. An empty list shows a single greyed placeholder.
///
/// Row labels deliberately use `tab_display_name` (custom-name / untitled-seq
/// aware, and sanitized) rather than Win32 `refresh_window_menu`'s raw
/// `path.file_name()` + numeric prefix: it is the same name the tab strip
/// and title show, so the switcher stays consistent with them. This is a
/// deliberate improvement over the Win32 label, not an oversight to "fix"
/// back to parity.
fn populate_open_files_menu(menu: &gtk::Menu) {
    for child in menu.children() {
        menu.remove(&child);
    }
    let rows = with_state(|st| {
        let active = st.shell.active_tab;
        st.shell
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id, codepp_shell::tab_display_name(t), active == Some(i)))
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    if rows.is_empty() {
        let empty = gtk::MenuItem::with_label("(no open files)");
        empty.set_sensitive(false);
        menu.append(&empty);
    } else {
        for (id, name, is_active) in rows {
            // `tab_display_name` already sanitizes control/bidi characters
            // for display, and `with_label` does not parse mnemonics, so the
            // filename reaches the menu verbatim and inert.
            let item = gtk::CheckMenuItem::with_label(&name);
            item.set_draw_as_radio(true);
            item.set_active(is_active);
            item.connect_activate(move |_| crate::select_tab_by_id(id));
            menu.append(&item);
        }
    }
    menu.show_all();
}

/// Fetch a top-level menu's submenu by its position in the bar.
///
/// `build` populates the bar in a fixed order, so position is a stable
/// handle without threading each `gtk::Menu` through the state struct.
/// Returns `None` — logged — rather than panicking if the bar is not
/// the shape `build` produced, since a menu that fails to wire is a
/// degraded UI, not a crash.
fn submenu_at(bar: &gtk::MenuBar, index: usize, name: &str) -> Option<gtk::Menu> {
    let root = bar
        .children()
        .get(index)
        .and_then(|c| c.clone().downcast::<gtk::MenuItem>().ok());
    let Some(root) = root else {
        tracing::error!(index, name, "menu bar is missing a top-level item");
        return None;
    };
    let sub = root.submenu().and_then(|m| m.downcast::<gtk::Menu>().ok());
    if sub.is_none() {
        tracing::error!(name, "top-level menu item has no submenu");
    }
    sub
}

/// Populate every top-level menu and bind its accelerators.
///
/// Split from [`build`] so the window is fully constructed and the
/// state installed before any handler can possibly fire.
///
/// Accelerators mirror the Win32 backend's `CreateAcceleratorTableW`
/// block, DESIGN.md §7.5's source of truth. The edit shortcuts
/// (Undo/Cut/Copy/…) are live GTK accelerators here rather than left to
/// Scintilla's keymap as on Win32, but they route to the identical
/// `SCI_*` command, so the user-visible behaviour matches. GTK dispatches
/// a window accelerator before the focused widget, so exactly one action
/// fires — no double-undo — and the main window's only editable widget is
/// the Scintilla view, so the routing target is never ambiguous. The
/// modeless Find/Replace and modal Goto dialogs are separate windows with
/// their own focus, so their text entries keep their own Ctrl+C/V.
pub fn connect() {
    let Some((bar, window)) = with_state(|st| (st.menu_bar.clone(), st.window.clone())) else {
        return;
    };
    let accel = gtk::AccelGroup::new();
    window.add_accel_group(&accel);

    build_file_menu(&bar, &accel);
    build_edit_menu(&bar, &accel);
    build_search_menu(&bar, &accel);
    build_view_menu(&bar, &accel);
    build_encoding_menu(&bar);
    build_language_menu(&bar, &window);
    build_settings_menu(&bar, &window);
    build_plugins_menu(&bar);
    build_help_menu(&bar, &accel, &window);
}

/// Build the Plugins menu. Contents are lazy: the `show` handler loads
/// every pending plugin on first open (deferred load — DESIGN.md §6.4)
/// and rebuilds the per-plugin submenus from the loaded set. A greyed
/// placeholder shows until then (and whenever no plugin is installed).
fn build_plugins_menu(bar: &gtk::MenuBar) {
    let Some(menu) = submenu_at(bar, 7, "Plugins") else {
        return;
    };
    menu.connect_show(crate::plugin::ensure_loaded_and_rebuild);
    let placeholder = gtk::MenuItem::with_label("No plugins loaded");
    placeholder.set_sensitive(false);
    menu.append(&placeholder);
    menu.show_all();
}

fn build_file_menu(bar: &gtk::MenuBar, accel: &gtk::AccelGroup) {
    let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
    let ctrl_shift = ctrl | gtk::gdk::ModifierType::SHIFT_MASK;
    // MOD1 is Alt. Save As is Ctrl+Alt+S (Win32 parity), which frees
    // Ctrl+Shift+S for Save All.
    let ctrl_alt = ctrl | gtk::gdk::ModifierType::MOD1_MASK;
    // Order mirrors Win32's File menu (`build_main_menu`): New, Open, Open
    // Folder as Workspace, Reload from Disk, Save, Save As, Save All, Rename,
    // Close, Close All. Open Folder as Workspace and Rename are plain `fn()`
    // actions, so they live in this array rather than as separate appends.
    let entries = [
        Entry {
            label: "_New",
            accel: Some((key::n, ctrl)),
            action: on_new,
        },
        Entry {
            label: "_Open…",
            accel: Some((key::o, ctrl)),
            action: on_open,
        },
        Entry {
            label: "Open Folder as _Workspace…",
            accel: None,
            action: crate::workspace::open_folder_flow,
        },
        Entry {
            label: "_Reload from Disk",
            accel: Some((key::r, ctrl)),
            action: on_reload,
        },
        Entry {
            label: "_Save",
            accel: Some((key::s, ctrl)),
            action: on_save,
        },
        Entry {
            label: "Save _As…",
            accel: Some((key::s, ctrl_alt)),
            action: on_save_as,
        },
        Entry {
            label: "Sa_ve All",
            accel: Some((key::S, ctrl_shift)),
            action: on_save_all,
        },
        Entry {
            label: "Rena_me…",
            accel: None,
            action: on_rename,
        },
        Entry {
            label: "_Close",
            accel: Some((key::w, ctrl)),
            action: on_close,
        },
        Entry {
            label: "Close A_ll",
            accel: Some((key::W, ctrl_shift)),
            action: on_close_all,
        },
    ];
    let Some(menu) = submenu_at(bar, 0, "File") else {
        return;
    };
    populate(&menu, accel, &entries);
    insert_open_containing_folder(&menu);
    insert_open_in_default_viewer(&menu);
    // Appends after Close All (the current last child), matching Win32's
    // placement directly below it.
    insert_close_multiple_documents(&menu);
    // Directly below Close Multiple Documents, before the session separator —
    // Win32's placement.
    insert_move_to_recycle_bin(&menu);
    menu.append(&gtk::SeparatorMenuItem::new());
    build_file_menu_lower(&menu, accel);
    menu.show_all();
}

/// Insert "Move to Recycle Bin" below Close Multiple Documents, matching
/// Win32. Moves the active buffer's on-disk file to the desktop Trash (the
/// freedesktop analogue of the Recycle Bin) after a confirmation, then closes
/// the buffer. Greyed while the active buffer is untitled — refreshed on
/// File-menu open — so it lives here rather than in the static `entries`
/// array.
fn insert_move_to_recycle_bin(menu: &gtk::Menu) {
    // Mnemonic on `b` (free in this flat File menu).
    let item = gtk::MenuItem::with_mnemonic("Move to Recycle _Bin");
    item.connect_activate(|_| on_move_to_recycle_bin());
    menu.append(&item);
    menu.connect_show(move |_| {
        let has_path =
            with_state(|st| st.shell.active().is_some_and(|t| t.path.is_some())).unwrap_or(false);
        item.set_sensitive(has_path);
    });
}

/// File → Move to Recycle Bin: after confirming, move the active buffer's
/// on-disk file to the desktop Trash (`g_file_trash`) and close the buffer —
/// the GTK analogue of Win32's `SHFileOperation(FO_DELETE | FOF_ALLOWUNDO)`.
///
/// Order: confirm → trash → close (only on trash success). This deliberately
/// reverses Win32's close-then-trash: `g_file_trash` fails on filesystems with
/// no trash support (an NTFS mount, say), and trashing first means a failure
/// leaves the buffer open and the file intact instead of closing the tab out
/// from under a file that never moved. A no-op for an untitled buffer (also
/// greyed). The buffer's unsaved edits are discarded without a save prompt —
/// the user's confirmation is that consent.
fn on_move_to_recycle_bin() {
    // Snapshot the active buffer's stable id + on-disk path together.
    let Some((id, path)) = with_state(|st| {
        st.shell
            .active()
            .and_then(|t| t.path.clone().map(|p| (t.id, p)))
    })
    .flatten() else {
        return;
    };

    // Confirm — this discards the buffer's unsaved edits and moves the file.
    // `message_dialog` parents itself to the main window via `with_state`.
    let body = format!(
        "The file “{}” will be moved to the Recycle Bin (Trash) and this \
         document will be closed.\nContinue?",
        codepp_shell::sanitize_path_for_display(&path)
    );
    let resp = crate::message_dialog(
        gtk::MessageType::Question,
        gtk::ButtonsType::OkCancel,
        "Move to Recycle Bin",
        &body,
    );
    if resp != gtk::ResponseType::Ok {
        return;
    }

    // Guard the destructive step against a worker wake that activated a
    // different tab while the modal was up: only proceed if the active buffer
    // is still the one we prompted about. Keyed on the stable `Tab.id` (not the
    // path — ids are never reused, so this can't be fooled by a different tab
    // that happens to share the path). Abort otherwise — closing/trashing the
    // wrong buffer would be data loss.
    let still_active =
        with_state(|st| st.shell.active().map(|t| t.id) == Some(id)).unwrap_or(false);
    if !still_active {
        return;
    }

    // Trash FIRST, then close only on success — the reverse of Win32's order,
    // deliberately: `g_file_trash` fails on filesystems with no trash support
    // (e.g. an NTFS mount from a dual-boot Windows install, which has no
    // `.Trash-1000` directory), and closing the buffer before finding that out
    // would leave the user with the file still on disk but its tab gone. This
    // way a failed trash is a clean no-op: the buffer stays open, nothing is
    // lost, and the error explains why.
    //
    // `trash()` runs synchronously on the UI thread. Accepted: a
    // same-filesystem trash is a rename (effectively instant), the common case;
    // a cross-filesystem or slow-media trash (GVfs's copy+delete fallback)
    // could stall the UI briefly, tolerable for a single, user-initiated,
    // already-confirmed delete. `g_file_trash_async` is the lever if needed.
    if let Err(err) = gio::File::for_path(&path).trash(gio::Cancellable::NONE) {
        crate::message_dialog(
            gtk::MessageType::Error,
            gtk::ButtonsType::Ok,
            "Move to Recycle Bin failed",
            &codepp_shell::sanitize_str_for_display(&err.to_string()),
        );
        return;
    }

    // The file is gone; now close the buffer. Suppress the close path's save
    // prompt — the file and its edits are being discarded, so prompting to save
    // is nonsensical (Win32 does the same via SCI_SETSAVEPOINT). The editor is
    // bound to the active document, so this marks exactly that buffer clean.
    with_state(|st| {
        st.editor.send(codepp_scintilla_sys::SCI_SETSAVEPOINT, 0, 0);
    });
    // SCI_SETSAVEPOINT re-enters `refresh_active_dirty` synchronously via
    // `sci-notify` while the borrow above is held, so the cached `Tab.dirty`
    // isn't refreshed there. Refresh it now, with the borrow dropped, so
    // `confirm_discard_active` — which ORs the cache with `SCI_GETMODIFY` —
    // sees the buffer clean and skips the redundant "Save changes?" prompt.
    // Same resync `on_save` does after `mark_saved`.
    refresh_tab_chrome();
    if !close_active_tab() {
        // The close can only decline here via a re-entrant `with_state`
        // (vanishingly rare, and the prompt is already suppressed). The file is
        // already trashed, so the tab is left pointing at a deleted path — a
        // later Save would simply recreate it. Log rather than swallow it.
        tracing::warn!(
            ?path,
            "recycle-bin close declined after trash; buffer left open on a now-deleted path"
        );
    }
}

/// Append the "Close Multiple Documents" submenu after Close All, matching
/// Win32. The five variants each close a subset of tabs through
/// [`crate::close_multiple_documents`] (per-tab dirty prompt; Cancel aborts).
/// Each item is greyed independently on File-menu open via the shared
/// `codepp_shell::close_multi_enabled`, so the greying agrees with what the
/// close loop — driven by the same crate's `pick_next_close_target` — does.
fn insert_close_multiple_documents(menu: &gtk::Menu) {
    use codepp_shell::CloseMultiKind::{
        AllButActive, AllButPinned, AllToLeft, AllToRight, AllUnchanged,
    };
    // Labels + mnemonics (A / P / L / R / U) match Win32 and are scoped to
    // this popup, so they can't clash with the top-level File-menu mnemonics.
    let rows = [
        ("Close All but _Active Document", AllButActive),
        ("Close All but _Pinned Documents", AllButPinned),
        ("Close All To the _Left", AllToLeft),
        ("Close All To the _Right", AllToRight),
        ("Close All _Unchanged", AllUnchanged),
    ];
    let submenu = gtk::Menu::new();
    let mut items: Vec<(gtk::MenuItem, codepp_shell::CloseMultiKind)> = Vec::new();
    for (label, kind) in rows {
        let item = gtk::MenuItem::with_mnemonic(label);
        item.connect_activate(move |_| crate::close_multiple_documents(kind));
        submenu.append(&item);
        items.push((item, kind));
    }

    // Mnemonic on `u` — `_M`ultiple would clash with `Rena_me`, and it is the
    // only otherwise-unclaimed letter of the label in this flat File menu.
    let parent = gtk::MenuItem::with_mnemonic("Close M_ultiple Documents");
    parent.set_submenu(Some(&submenu));
    menu.append(&parent);

    // Grey each entry per the shared predicate whenever the File menu opens.
    menu.connect_show(move |_| {
        with_state(|st| {
            for (item, kind) in &items {
                let enabled =
                    codepp_shell::close_multi_enabled(&st.shell.tabs, st.shell.active_tab, *kind);
                item.set_sensitive(enabled);
            }
        });
    });
}

/// Which "Open Containing Folder" action to run on the active buffer.
#[derive(Clone, Copy)]
enum ContainingAction {
    /// Show the file selected (and scrolled into view) in the desktop file
    /// manager.
    FileManager,
    /// Open a terminal emulator with its working directory at the folder.
    Terminal,
    /// Root the workspace panel at the folder.
    Workspace,
}

/// Insert the "Open Containing Folder" submenu directly after Open, matching
/// Win32's order. The three actions are resolved against the active buffer at
/// click time — File Explorer selects the file, Terminal and Folder as
/// Workspace act on its parent directory. The whole submenu is greyed while
/// the active buffer has no on-disk parent (untitled), refreshed on File-menu
/// open — so it lives here rather than in the static `entries` array.
///
/// Win32's submenu is Explorer / cmd / PowerShell / — / Folder as Workspace;
/// the GTK equivalents are File Explorer and Terminal (there is no single
/// Windows-shell analogue), plus the shared Folder as Workspace.
fn insert_open_containing_folder(menu: &gtk::Menu) {
    let submenu = gtk::Menu::new();
    // Submenu mnemonics (E / T / W) are scoped to this popup, so they cannot
    // clash with the top-level File-menu mnemonics.
    let explorer = gtk::MenuItem::with_mnemonic("File _Explorer");
    explorer.connect_activate(|_| open_containing(ContainingAction::FileManager));
    submenu.append(&explorer);
    let terminal = gtk::MenuItem::with_mnemonic("_Terminal");
    terminal.connect_activate(|_| open_containing(ContainingAction::Terminal));
    submenu.append(&terminal);
    submenu.append(&gtk::SeparatorMenuItem::new());
    let workspace = gtk::MenuItem::with_mnemonic("Folder as _Workspace");
    workspace.connect_activate(|_| open_containing(ContainingAction::Workspace));
    submenu.append(&workspace);

    // Mnemonic on `g` — `_C`ontaining would clash with `_Close`, and every
    // other letter of the label is already claimed in this flat File menu.
    let parent = gtk::MenuItem::with_mnemonic("Open Containin_g Folder");
    parent.set_submenu(Some(&submenu));
    // Position 2: after New (0) and Open (1); the ODV insert that follows
    // lands at 3, keeping New, Open, Open Containing Folder, Open in Default
    // Viewer, Open Folder as Workspace — Win32's order.
    menu.insert(&parent, 2);
    menu.connect_show(move |_| {
        let has_parent = with_state(|st| {
            st.shell
                .active()
                .and_then(|t| t.path.as_deref())
                .and_then(Path::parent)
                .is_some()
        })
        .unwrap_or(false);
        parent.set_sensitive(has_parent);
    });
}

/// Run an "Open Containing Folder" action against the active buffer's file
/// (resolved now, at click time). A no-op for an untitled buffer or a path
/// with no parent — the submenu is also greyed in that state. File Explorer
/// acts on the file itself (to select it); Terminal and Folder as Workspace
/// act on its parent directory.
fn open_containing(action: ContainingAction) {
    let file = with_state(|st| st.shell.active().and_then(|t| t.path.clone())).flatten();
    let Some(file) = file else {
        return;
    };
    let Some(dir) = file.parent().map(Path::to_path_buf) else {
        return;
    };
    match action {
        ContainingAction::FileManager => show_file_in_manager(&file),
        ContainingAction::Terminal => open_terminal_in(&dir),
        ContainingAction::Workspace => crate::workspace::open_at(&dir),
    }
}

/// Timeout (ms) for the `ShowItems` D-Bus call before its fallback runs. The
/// call is asynchronous, so the UI never blocks regardless — this only bounds
/// how long a hung or absent file manager delays the folder-open fallback.
const SHOW_ITEMS_TIMEOUT_MS: i32 = 3000;

/// Show `file` selected — and scrolled into view — in the desktop file
/// manager, the Linux analogue of Win32's `explorer /select,`. Uses the
/// freedesktop `org.freedesktop.FileManager1.ShowItems` D-Bus method, which
/// Nautilus, Dolphin, Nemo, Caja, Thunar and `PCManFM` all implement.
///
/// The `ShowItems` call is asynchronous, so the GTK thread does not block on
/// the file manager itself (which may need D-Bus activation); only the
/// one-off `bus_get_sync` session-bus handshake is synchronous — a local
/// Unix-socket round trip in the sub-millisecond range, on par with the other
/// synchronous glib calls in this file. If the interface is unavailable or
/// the call errors, fall back to opening the *containing folder* (no
/// selection) with the default handler — every file manager handles that. The
/// URI is built by `filename_to_uri` (percent-escaped) and passed as method
/// data, never to a shell.
fn show_file_in_manager(file: &Path) {
    let Ok(uri) = glib::filename_to_uri(file, None) else {
        tracing::warn!(?file, "show in file manager: filename_to_uri failed");
        return;
    };
    // Captured for the async fallback (and the no-bus fallback below).
    let parent = file.parent().map(Path::to_path_buf);
    let window = with_state(|st| st.window.clone());

    let conn = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!(?e, "no session bus; opening the containing folder instead");
            open_containing_folder_fallback(parent.as_deref(), window.as_ref());
            return;
        }
    };
    let params = (vec![uri.to_string()], String::new()).to_variant();
    conn.call(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
        "ShowItems",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        SHOW_ITEMS_TIMEOUT_MS,
        gio::Cancellable::NONE,
        move |res| {
            if let Err(e) = res {
                tracing::warn!(
                    ?e,
                    "FileManager1.ShowItems failed; opening the folder instead"
                );
                open_containing_folder_fallback(parent.as_deref(), window.as_ref());
            }
        },
    );
}

/// Fallback for [`show_file_in_manager`]: open the containing folder itself
/// (no file selection) through the default handler — the same primitive as
/// Open in Default Viewer, on the directory.
fn open_containing_folder_fallback(dir: Option<&Path>, window: Option<&gtk::Window>) {
    let (Some(dir), Some(window)) = (dir, window) else {
        return;
    };
    match glib::filename_to_uri(dir, None) {
        Ok(uri) => open_uri(window, &uri),
        Err(e) => tracing::warn!(
            ?e,
            "open containing folder fallback: filename_to_uri failed"
        ),
    }
}

/// Launch a terminal emulator with its working directory at `dir`. Linux has
/// no single "open a terminal here" standard, so try a prioritised list of
/// the common emulators, each spawned with `dir` as its working directory —
/// the interactive shell it starts inherits that directory. The directory is
/// never passed as a command argument, so a folder name containing shell
/// metacharacters cannot inject anything (there is no shell in the chain).
/// The child is reaped in a detached thread so a closed terminal leaves no
/// zombie.
fn open_terminal_in(dir: &Path) {
    // `x-terminal-emulator` is the Debian/Ubuntu alternatives symlink to the
    // user's chosen terminal; the rest cover the common desktops.
    const TERMINALS: &[&str] = &[
        "x-terminal-emulator",
        "gnome-terminal",
        "konsole",
        "xfce4-terminal",
        "kitty",
        "alacritty",
        "xterm",
    ];
    for term in TERMINALS {
        if let Ok(mut child) = std::process::Command::new(term).current_dir(dir).spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return;
        }
    }
    tracing::warn!(
        ?dir,
        "open terminal: no known terminal emulator found on PATH"
    );
}

/// Insert "Open in Default Viewer" directly after Open, matching Win32's
/// order. Launches the active buffer's on-disk file with its OS-associated
/// application. Greyed while the active buffer has no path (untitled), so it
/// lives here rather than in the static `entries` array — its sensitivity is
/// refreshed on every File-menu open.
fn insert_open_in_default_viewer(menu: &gtk::Menu) {
    // Mnemonic on `e` — in this flat File menu `_f` clashes with the
    // "Recent _Files" submenu (shown when the In-Submenu recent-files pref is
    // on), `_D`efault with `Loa_d Session`, and `_V`iewer with `Sa_ve All`.
    let odv = gtk::MenuItem::with_mnemonic("Open in Default Vi_ewer");
    odv.connect_activate(|_| on_open_in_default_viewer());
    // Position 3: after New (0), Open (1) and Open Containing Folder (2),
    // before Open Folder as Workspace.
    menu.insert(&odv, 3);
    menu.connect_show(move |_| {
        let has_path =
            with_state(|st| st.shell.active().is_some_and(|t| t.path.is_some())).unwrap_or(false);
        odv.set_sensitive(has_path);
    });
}

/// File → Open in Default Viewer: hand the active buffer's on-disk file to
/// the desktop's associated application, the GTK analogue of Win32's
/// `ShellExecuteW("open", path)`. `show_uri_on_window` routes a `file://`
/// URI through the default handler for its content type — no shell is
/// involved, and `filename_to_uri` escapes the path, so a filename with
/// shell metacharacters cannot inject anything (same envelope as Win32's
/// `"open"` verb). A no-op for an untitled buffer (also greyed); a missing
/// handler surfaces as a logged warning in `open_uri`.
fn on_open_in_default_viewer() {
    let Some((Some(path), window)) = with_state(|st| {
        (
            st.shell.active().and_then(|t| t.path.clone()),
            st.window.clone(),
        )
    }) else {
        return;
    };
    match glib::filename_to_uri(&path, None) {
        Ok(uri) => open_uri(&window, &uri),
        Err(e) => tracing::warn!(?e, "open_in_default_viewer: filename_to_uri failed"),
    }
}

/// The lower half of the File menu, below the first separator: Load/Save
/// Session, Print, the recent-files region (with its always-on Ctrl+Shift+T
/// binding), and Exit. Split out of [`build_file_menu`] purely for length.
fn build_file_menu_lower(menu: &gtk::Menu, accel: &gtk::AccelGroup) {
    let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
    let ctrl_shift = ctrl | gtk::gdk::ModifierType::SHIFT_MASK;

    // Session interchange (Notepad++-shape XML at a user-picked path,
    // distinct from the automatic session.xml restore). Enabled at all
    // times — Load prompts regardless of what's open; Save writes even a
    // zero-tab session. No accelerators, matching Win32.
    // Mnemonic on the `d` — `_L` is already claimed by `Close A_ll`'s `l`,
    // which this File menu is careful to avoid colliding with elsewhere.
    let load_session = gtk::MenuItem::with_mnemonic("Loa_d Session…");
    load_session.connect_activate(|_| on_load_session());
    menu.append(&load_session);
    let save_session = gtk::MenuItem::with_mnemonic("Save Sess_ion…");
    save_session.connect_activate(|_| on_save_session());
    menu.append(&save_session);
    menu.append(&gtk::SeparatorMenuItem::new());

    // Print the active buffer (Ctrl+P).
    let print = gtk::MenuItem::with_mnemonic("_Print…");
    print.add_accelerator("activate", accel, *key::p, ctrl, gtk::AccelFlags::VISIBLE);
    print.connect_activate(|_| crate::print::show());
    menu.append(&print);

    // Print Now: straight to the default printer, no dialog. Placed directly
    // below Print, mirroring Win32's print cluster. No ellipsis (it never
    // prompts) and no accelerator — matching Win32, where the "just print it"
    // action is menu-only so a stray Ctrl-chord can't fire a job by accident.
    // `N` matches the key Win32 uses (`Print &Now`), and inherits the same
    // collision Win32 has: `_New` already claims `N` in this flat menu, and
    // every other letter of "Print Now" is taken too (P=Print, O=Open,
    // R=Reload, W=Close/Workspace, I=Save Session, T=Restore). So the item is
    // click-reachable but not mnemonic-reachable — an accepted parity quirk,
    // not a free key.
    let print_now = gtk::MenuItem::with_mnemonic("Print _Now");
    print_now.connect_activate(|_| crate::print::print_now());
    menu.append(&print_now);

    // Ctrl+Shift+T (Restore Recent Closed File) is registered directly on the
    // accel group so it works from startup — the menu item that echoes it is
    // rebuilt inside the recent region (below), and a rebuilt item's binding
    // would only exist after the File menu had first been opened. The item
    // shows the hint via a display-only accel group (see `FILE_HINT_ACCEL`),
    // which never routes the key, so there is no double binding.
    accel.connect_accel_group(
        *key::T,
        ctrl_shift,
        gtk::AccelFlags::VISIBLE,
        |_, _, _, _| {
            restore_recent_closed();
            true
        },
    );
    let hint = gtk::AccelGroup::new();
    FILE_HINT_ACCEL.with(|h| *h.borrow_mut() = Some(hint));

    // The recent-files region — the numbered file list, then Restore Recent
    // Closed File / Open All / Empty — is rebuilt on every File-menu open: its
    // contents are dynamic, and its shape follows the Preferences "In Submenu"
    // setting (inline flat by default, or nested in a "Recent Files" submenu).
    // It is inserted just above this anchor separator, which sits directly
    // above Exit; see `rebuild_recent_region`. Matches Win32's placement of
    // the recent region after Print and its Restore/Open All/Empty order.
    let anchor = gtk::SeparatorMenuItem::new();
    menu.append(&anchor);
    RECENT_ANCHOR.with(|a| *a.borrow_mut() = Some(anchor));
    menu.connect_show(rebuild_recent_region);

    // Exit stays at the bottom; Alt+F4 is the conventional close accelerator
    // and is shown as its hint (the window manager typically also maps it to
    // the window's delete path, which saves + quits the same way).
    let exit = gtk::MenuItem::with_mnemonic("E_xit");
    exit.add_accelerator(
        "activate",
        accel,
        *key::F4,
        gtk::gdk::ModifierType::MOD1_MASK,
        gtk::AccelFlags::VISIBLE,
    );
    exit.connect_activate(|_| {
        save_session_now();
        gtk::main_quit();
    });
    menu.append(&exit);
}

thread_local! {
    /// The persistent separator anchoring the recent-files region's lower
    /// edge; the region is rebuilt just above it on each File-menu open.
    /// Set once by [`build_file_menu`].
    static RECENT_ANCHOR: std::cell::RefCell<Option<gtk::SeparatorMenuItem>> =
        const { std::cell::RefCell::new(None) };
    /// The region items inserted at the last rebuild, removed at the next.
    /// Tracked so a rebuild removes exactly what it added, leaving the
    /// static File-menu items untouched. Same discipline as Win32's
    /// `recent_count` bookkeeping in `rebuild_file_menu_recent_region`.
    static RECENT_REGION: std::cell::RefCell<Vec<gtk::Widget>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// A display-only accel group for the rebuilt Restore Recent Closed File
    /// item's `Ctrl+Shift+T` hint. Deliberately never added to any window, so
    /// `add_accelerator` on it renders the shortcut label without routing the
    /// key — the real, always-on binding is the persistent
    /// `connect_accel_group` in [`build_file_menu`]. Set once there.
    static FILE_HINT_ACCEL: std::cell::RefCell<Option<gtk::AccelGroup>> =
        const { std::cell::RefCell::new(None) };
}

/// Rebuild the recent-files region on the File menu, respecting the
/// Preferences "In Submenu" setting. Mirror of Win32's
/// `rebuild_file_menu_recent_region`: remove the previous region, then
/// insert the fresh one just above the anchor separator. Runs on every
/// File-menu `show`.
fn rebuild_recent_region(menu: &gtk::Menu) {
    // Remove exactly the widgets the previous rebuild inserted.
    RECENT_REGION.with(|r| {
        for w in r.borrow_mut().drain(..) {
            menu.remove(&w);
        }
    });

    let Some(anchor) = RECENT_ANCHOR.with(|a| a.borrow().clone()) else {
        return;
    };
    let anchor: gtk::Widget = anchor.upcast();
    let Some(base) = menu.children().iter().position(|c| *c == anchor) else {
        return;
    };

    let items = recent_region_items();
    for (offset, item) in items.iter().enumerate() {
        // Insert above the anchor; each prior insert pushed the anchor down
        // by one, so `base + offset` keeps the region ordered and contiguous.
        menu.insert(item, i32::try_from(base + offset).unwrap_or(i32::MAX));
    }
    // Hand ownership of the freshly-inserted widgets to the tracker so the
    // next rebuild can remove exactly these.
    RECENT_REGION.with(|r| *r.borrow_mut() = items);
    menu.show_all();
}

/// Build the recent-files region items for the current state + Preferences.
///
/// Layout mirrors Win32's `rebuild_file_menu_recent_region`: when the
/// feature is inactive the region is empty (so only the anchor separator
/// sits between Print and Exit); otherwise a leading separator is followed
/// by the numbered file list (formatted per the display mode), then — after
/// an inner separator when non-empty — Restore Recent Closed File / Open All
/// / Empty. With "In Submenu" on, all of that nests inside a single "Recent
/// Files" popup; off (the default) it is inlined flat on the File menu. The
/// region owns its leading separator so an inactive region never leaves two
/// adjacent separators above Exit.
fn recent_region_items() -> Vec<gtk::Widget> {
    let (recents, cfg) = with_state(|st| {
        (
            st.shell.visible_recent_files().to_vec(),
            st.shell.preferences.recent_files_history.clone(),
        )
    })
    .unwrap_or_default();

    // Feature off (unchecked, or a zero cap): render nothing — matching
    // Win32's `!cfg.is_active()` early return.
    if !cfg.is_active() {
        return Vec::new();
    }

    // Numbered file entries. `format!("{N}: {display}")` mirrors Win32's
    // `format_recent_menu_label` (its `&`-mnemonic is a Win32 accelerator
    // detail; the on-screen text is the same "N: name"). `with_label`, not
    // `with_mnemonic`: a filename's own `_` must not become an accelerator,
    // and the display string is already sanitised against hostile chars.
    let file_items: Vec<gtk::MenuItem> = recents
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let label = format!(
                "{}: {}",
                index + 1,
                codepp_shell::sanitize_str_for_display(&cfg.display_path(path))
            );
            let item = gtk::MenuItem::with_label(&label);
            item.connect_activate(move |_| open_recent_at(index));
            item
        })
        .collect();

    let has = !recents.is_empty();
    // Restore Recent Closed File, above Open All / Empty — Win32's order. Its
    // functional Ctrl+Shift+T lives on the accel group (see `build_file_menu`);
    // here it only carries the display-only hint accel so the shortcut shows.
    // Mnemonic on `t`, not `R` (which `_Reload from Disk` claims in the same
    // flat File menu), and it echoes the Ctrl+Shift+T shortcut.
    let restore = gtk::MenuItem::with_mnemonic("Res_tore Recent Closed File");
    restore.set_sensitive(has);
    if let Some(hint) = FILE_HINT_ACCEL.with(|h| h.borrow().clone()) {
        let ctrl_shift = gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::SHIFT_MASK;
        restore.add_accelerator(
            "activate",
            &hint,
            *key::T,
            ctrl_shift,
            gtk::AccelFlags::VISIBLE,
        );
    }
    restore.connect_activate(|_| restore_recent_closed());
    // No mnemonics on these two, matching Win32 (`inline_action_labels`) — and
    // it avoids an `O` clash with `_Open…` in the same flat menu.
    let open_all = gtk::MenuItem::with_label("Open All Recent Files");
    open_all.set_sensitive(has);
    open_all.connect_activate(|_| open_all_recent());
    let empty = gtk::MenuItem::with_label("Empty Recent Files List");
    empty.set_sensitive(has);
    empty.connect_activate(|_| empty_recent());

    if cfg.in_submenu {
        let submenu = gtk::Menu::new();
        for it in &file_items {
            submenu.append(it);
        }
        if has {
            submenu.append(&gtk::SeparatorMenuItem::new());
        }
        submenu.append(&restore);
        submenu.append(&open_all);
        submenu.append(&empty);
        let parent = gtk::MenuItem::with_mnemonic("Recent _Files");
        parent.set_submenu(Some(&submenu));
        // The region owns its leading separator (Print sits above it) so an
        // inactive region leaves exactly one separator before Exit, never two.
        vec![gtk::SeparatorMenuItem::new().upcast(), parent.upcast()]
    } else {
        // Leading separator first — see the submenu branch's note.
        let mut out: Vec<gtk::Widget> = vec![gtk::SeparatorMenuItem::new().upcast()];
        out.extend(file_items.into_iter().map(Cast::upcast));
        if has {
            out.push(gtk::SeparatorMenuItem::new().upcast());
        }
        out.push(restore.upcast());
        out.push(open_all.upcast());
        out.push(empty.upcast());
        out
    }
}

/// Open the recent-files entry at `index` (removing it from the list — it
/// is now open, and will re-enter on its next close).
///
/// `index` is captured when the submenu is (re)built on show; the GTK main
/// loop is single-threaded, so the list cannot change between show and
/// click, and `take_recent_file_at` re-validates the bound anyway (`None`
/// out of range, never a panic).
fn open_recent_at(index: usize) {
    let path = with_state(|st| st.shell.take_recent_file_at(index)).flatten();
    if let Some(path) = path {
        open_paths(vec![path]);
    }
}

/// Ctrl+Shift+T / Restore Recent Closed File: reopen the most-recently
/// closed file.
fn restore_recent_closed() {
    let path = with_state(|st| st.shell.pop_last_recent_file()).flatten();
    if let Some(path) = path {
        open_paths(vec![path]);
    }
}

/// Open every recent file, most-recent first, emptying the list.
fn open_all_recent() {
    let paths = with_state(|st| st.shell.take_all_recent_files()).unwrap_or_default();
    open_paths(paths);
}

/// Drop every tracked recent path.
fn empty_recent() {
    with_state(|st| st.shell.clear_recent_files());
}

/// File → Rename. A saved buffer routes to Save As (a real on-disk move to
/// the chosen path); an untitled buffer gets a display-name change through
/// a small modal, matching Win32's two-branch behaviour.
fn on_rename() {
    let has_path =
        with_state(|st| st.shell.active().is_some_and(|t| t.path.is_some())).unwrap_or(false);
    if has_path {
        on_save_as();
        return;
    }
    // Untitled: prompt for a display name, seeded with the current one.
    let current = with_state(|st| st.shell.active().map(codepp_shell::tab_display_name)).flatten();
    let Some(current) = current else {
        return;
    };
    if let Some(new_name) = prompt_rename(&current) {
        let changed = with_state(|st| st.shell.set_active_custom_name(&new_name)).unwrap_or(false);
        if changed {
            sync_tab_strip();
            refresh_tab_chrome();
        }
    }
}

/// Modal name prompt for renaming an untitled buffer. Returns the entered
/// text on OK (empty string clears the name back to `new N`), `None` on
/// Cancel. Mirrors the Goto dialog's shape.
fn prompt_rename(current: &str) -> Option<String> {
    let parent = with_state(|st| st.window.clone());
    let dialog = gtk::Dialog::with_buttons(
        Some("Rename"),
        parent.as_ref(),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[
            ("_Cancel", gtk::ResponseType::Cancel),
            ("_Rename", gtk::ResponseType::Accept),
        ],
    );
    dialog.set_default_response(gtk::ResponseType::Accept);
    let content = dialog.content_area();
    content.set_spacing(6);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(8);
    content.set_margin_end(8);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.pack_start(&gtk::Label::new(Some("Name:")), false, false, 0);
    let entry = gtk::Entry::new();
    entry.set_text(current);
    entry.set_activates_default(true);
    entry.set_width_chars(28);
    row.pack_start(&entry, true, true, 0);
    content.pack_start(&row, false, false, 0);
    dialog.show_all();

    let result = if dialog.run() == gtk::ResponseType::Accept {
        Some(entry.text().to_string())
    } else {
        None
    };
    // SAFETY: created here, never handed out — same as the Goto dialog.
    unsafe {
        dialog.destroy();
    }
    result
}

/// State of Notepad++'s "Begin/End Select" feature. See
/// [`on_begin_end_select`] for the state machine and [`refresh_edit_menu`]
/// for the menu-side check + grey indicator. Lives on
/// [`crate::state::GtkUiState::select_mark`]; the Win32 backend carries
/// an equivalent [`SelectMarkMode`](../../ui_win32/src/lib.rs) enum,
/// deliberately duplicated rather than shared because the whole feature
/// is UI-mode state that never leaves its own backend.
///
/// The armed variants carry the originating tab's monotonic id (from
/// [`codepp_shell::Shell::allocate_buffer_id`], never reused) alongside
/// the anchor byte position. [`resolve_begin_end_step`] verifies both
/// against the active tab and its current document length before
/// applying the selection — a stale anchor from a closed / replaced /
/// shrunk buffer resolves to [`SelectStep::Invalidated`] and disarms
/// silently instead of feeding a nonsense range to `SCI_SETSEL`.
/// Same self-healing shape the tab-arm-commit fix uses (DESIGN.md §7.4,
/// `resolve_tab_arm_commit`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SelectMarkMode {
    /// No selection currently armed.
    None,
    /// Stream (contiguous) selection armed. `anchor` is the caret byte
    /// position at "Begin" time; `tab_id` is the [`codepp_shell::Tab`]
    /// id that was active then.
    Stream { anchor: isize, tab_id: i32 },
    /// Rectangular (column) selection armed. As above.
    Column { anchor: isize, tab_id: i32 },
}

/// Output of the Begin/End Select state machine. Kept as a plain enum so
/// [`resolve_begin_end_step`] can be a pure function of the four inputs
/// (current mode, pressed variant, active tab id, active doc length),
/// unit-testable without an editor widget or a GTK display. The handler
/// interprets each variant against the live editor.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum SelectStep {
    /// Fresh first press — capture the current caret and arm the given
    /// variant.
    Begin { column: bool },
    /// Matching second press — apply `SCI_SETSEL(anchor, current)` in
    /// stream / rectangular mode and disarm.
    End { anchor: isize, column: bool },
    /// Armed variant matched, but the anchor is no longer valid for the
    /// current buffer (tab closed/replaced, buffer shrunk past the
    /// anchor). Silently disarm — no selection is applied. Prevents the
    /// clamped-but-wrong `SCI_SETSEL(stale_anchor, current)` outcome
    /// the security audit flagged as data-loss adjacent.
    Invalidated,
    /// Sibling variant is armed (Stream when Column pressed, or vice
    /// versa). Leave the arm alone — the greyed menu entry already
    /// blocks this via mouse, and on Win32 the accel-table still fires
    /// the `WM_COMMAND`, so the pure step exists to make both backends
    /// no-op symmetrically.
    Ignored,
}

/// Pure state transition for Begin/End Select. See [`SelectStep`] for
/// the four outputs. Broken out from the handler so the transition can
/// be tested exhaustively without spinning up a widget — matches the
/// [`resolve_tab_arm_commit`] precedent (DESIGN.md §7.4).
#[must_use]
pub(crate) fn resolve_begin_end_step(
    current: SelectMarkMode,
    press_column: bool,
    active_tab_id: i32,
    active_doc_length: isize,
) -> SelectStep {
    match (current, press_column) {
        (SelectMarkMode::None, column) => SelectStep::Begin { column },
        (SelectMarkMode::Stream { anchor, tab_id }, false) => {
            if tab_id == active_tab_id && anchor >= 0 && anchor <= active_doc_length {
                SelectStep::End {
                    anchor,
                    column: false,
                }
            } else {
                SelectStep::Invalidated
            }
        }
        (SelectMarkMode::Column { anchor, tab_id }, true) => {
            if tab_id == active_tab_id && anchor >= 0 && anchor <= active_doc_length {
                SelectStep::End {
                    anchor,
                    column: true,
                }
            } else {
                SelectStep::Invalidated
            }
        }
        (SelectMarkMode::Stream { .. } | SelectMarkMode::Column { .. }, _) => SelectStep::Ignored,
    }
}

thread_local! {
    /// The Edit menu's "Begin/End Select" `CheckMenuItem`, stashed at
    /// build time so [`refresh_edit_menu`] can flip its check and
    /// sensitivity in place. Same pattern as [`RECENT_ANCHOR`].
    static BEGIN_END_SELECT_ITEM: std::cell::RefCell<Option<gtk::CheckMenuItem>> =
        const { std::cell::RefCell::new(None) };
    /// The column-mode sibling of [`BEGIN_END_SELECT_ITEM`].
    static BEGIN_END_SELECT_COLUMN_ITEM: std::cell::RefCell<Option<gtk::CheckMenuItem>> =
        const { std::cell::RefCell::new(None) };
    /// Reentrancy guard for [`refresh_edit_menu`]. `set_active` on a
    /// `GtkCheckMenuItem` calls `gtk_menu_item_activate()` internally
    /// when the value actually changes, which re-emits `activate` and
    /// would re-enter [`on_begin_end_select`] mid-refresh — unbounded
    /// recursion, easily triggered by a tab switch on an armed mode.
    /// The handler bails while this is set. Same pattern as
    /// [`REFRESHING_MARKS`], which exists for the same reason on the
    /// Encoding / Language menus.
    static REFRESHING_EDIT_MENU_MARKS: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// The Edit menu — Win32's minimal Scintilla-backed set. Delete carries
/// no application accelerator: the Del key stays with Scintilla for
/// normal forward-delete; the menu item just exposes `SCI_CLEAR`.
///
/// Undo and Redo advertise their dual bindings (Ctrl+Z / Alt+Backspace,
/// Ctrl+Y / Ctrl+Shift+Z) via a custom hint label — `GtkAccelLabel` only
/// renders one binding per item, so the second is shown as static text.
/// All four keys are already routed by Scintilla's built-in keymap
/// (`KeyMap.cxx`), so the menu-level accelerator only needs to bind the
/// primary one for menu-driven activation; the secondary key reaches
/// Scintilla directly. Select All sits directly below Delete with no
/// separator between them, matching Notepad++'s Edit-menu layout.
///
/// Below Select All sit the two Begin/End Select entries (Ctrl+Shift+B
/// stream, Alt+Shift+B column). Each is a [`gtk::CheckMenuItem`] whose
/// check mark reflects [`crate::state::GtkUiState::select_mark`] — the
/// currently-armed variant paints checked and its sibling greys out
/// (GTK's `set_sensitive(false)` blocks both mouse click *and*
/// accelerator dispatch, which matches the intended UX). Handles are
/// stashed in [`BEGIN_END_SELECT_ITEM`] / `_COLUMN_ITEM` so
/// [`refresh_edit_menu`] can update them from anywhere.
fn build_edit_menu(bar: &gtk::MenuBar, accel: &gtk::AccelGroup) {
    let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
    let ctrl_shift = ctrl | gtk::gdk::ModifierType::SHIFT_MASK;
    let alt_shift = gtk::gdk::ModifierType::MOD1_MASK | gtk::gdk::ModifierType::SHIFT_MASK;
    let Some(menu) = submenu_at(bar, 1, "Edit") else {
        return;
    };
    append_dual_accel_edit_item(
        &menu,
        accel,
        "_Undo",
        "Ctrl+Z / Alt+Backspace",
        (key::z, ctrl),
        on_undo,
    );
    append_dual_accel_edit_item(
        &menu,
        accel,
        "_Redo",
        "Ctrl+Y / Ctrl+Shift+Z",
        (key::y, ctrl),
        on_redo,
    );
    menu.append(&gtk::SeparatorMenuItem::new());
    let clip = [
        Entry {
            label: "Cu_t",
            accel: Some((key::x, ctrl)),
            action: on_cut,
        },
        Entry {
            label: "_Copy",
            accel: Some((key::c, ctrl)),
            action: on_copy,
        },
        Entry {
            label: "_Paste",
            accel: Some((key::v, ctrl)),
            action: on_paste,
        },
        Entry {
            label: "_Delete",
            accel: None,
            action: on_delete,
        },
        Entry {
            label: "Select _All",
            accel: Some((key::a, ctrl)),
            action: on_select_all,
        },
    ];
    populate(&menu, accel, &clip);
    // Begin/End Select — CheckMenuItem so the "armed" state renders as
    // a check mark. Connect to `activate` (fires only on user click or
    // accelerator; not on programmatic `set_active`) so the state
    // machine below is the sole driver of `select_mark`, and any
    // corrective `set_active` call is a silent visual sync.
    let begin_end = gtk::CheckMenuItem::with_mnemonic("_Begin/End Select");
    begin_end.add_accelerator(
        "activate",
        accel,
        *key::b,
        ctrl_shift,
        gtk::AccelFlags::VISIBLE,
    );
    begin_end.connect_activate(|_| on_begin_end_select(false));
    menu.append(&begin_end);
    let begin_end_column = gtk::CheckMenuItem::with_mnemonic("Begin/End Select in Column _Mode");
    begin_end_column.add_accelerator(
        "activate",
        accel,
        *key::b,
        alt_shift,
        gtk::AccelFlags::VISIBLE,
    );
    begin_end_column.connect_activate(|_| on_begin_end_select(true));
    menu.append(&begin_end_column);
    BEGIN_END_SELECT_ITEM.with(|c| *c.borrow_mut() = Some(begin_end));
    BEGIN_END_SELECT_COLUMN_ITEM.with(|c| *c.borrow_mut() = Some(begin_end_column));
    menu.show_all();
}

/// Toggle Begin/End Select for the active view. `column = false` is
/// stream mode (Ctrl+Shift+B); `true` is rectangular mode
/// (Alt+Shift+B).
///
/// Delegates the transition to [`resolve_begin_end_step`], then
/// interprets the resulting [`SelectStep`] against the live editor.
/// The step machine's tab-id + doc-length verification means an
/// anchor from a tab that was closed / replaced / reloaded-and-shrunk
/// resolves to [`SelectStep::Invalidated`] and disarms silently
/// rather than feeding a stale byte offset to `SCI_SETSEL`.
///
/// Guarded by [`REFRESHING_EDIT_MENU_MARKS`]: `set_active` on a
/// `GtkCheckMenuItem` calls `gtk_menu_item_activate()` internally when
/// the value actually changes, which re-emits `activate` and would
/// re-enter this handler mid-refresh. The guard makes such
/// reentrancy a no-op instead of an unbounded recursion; the earlier
/// audit caught a stack overflow triggered by a tab switch on an
/// armed mode driving exactly that reentrancy path.
pub(crate) fn on_begin_end_select(column: bool) {
    if REFRESHING_EDIT_MENU_MARKS.with(std::cell::Cell::get) {
        return;
    }
    with_state(|st| {
        let editor = st.editor;
        let active_tab_id = st.shell.active().map_or(-1, |t| t.id);
        let doc_length = editor.send(codepp_scintilla_sys::SCI_GETLENGTH, 0, 0);
        let step = resolve_begin_end_step(st.select_mark, column, active_tab_id, doc_length);
        st.select_mark = match step {
            SelectStep::Begin { column } => {
                let pos = editor.send(codepp_scintilla_sys::SCI_GETCURRENTPOS, 0, 0);
                if column {
                    SelectMarkMode::Column {
                        anchor: pos,
                        tab_id: active_tab_id,
                    }
                } else {
                    SelectMarkMode::Stream {
                        anchor: pos,
                        tab_id: active_tab_id,
                    }
                }
            }
            SelectStep::End { anchor, column } => {
                let current = editor.send(codepp_scintilla_sys::SCI_GETCURRENTPOS, 0, 0);
                let mode = if column {
                    codepp_scintilla_sys::SC_SEL_RECTANGLE
                } else {
                    codepp_scintilla_sys::SC_SEL_STREAM
                };
                // Order matters: Scintilla's `SCI_SETSEL` handler
                // (`Editor.cxx:6324-6337`) unconditionally forces
                // `sel.selType = stream` before applying the new
                // range, so a preceding `SCI_SETSELECTIONMODE` is
                // silently clobbered. Set the anchor/caret first,
                // then flip the selection mode — `SCI_SETSELECTIONMODE`
                // converts the *current* selection into the requested
                // shape (`Editor::SetSelectionMode` at
                // `Editor.cxx:6124`), which is what actually produces
                // a rectangular selection for column mode.
                //
                // `anchor` was gated `>= 0` by `resolve_begin_end_step`.
                #[allow(clippy::cast_sign_loss)]
                editor.send(codepp_scintilla_sys::SCI_SETSEL, anchor as usize, current);
                editor.send(codepp_scintilla_sys::SCI_SETSELECTIONMODE, mode as usize, 0);
                SelectMarkMode::None
            }
            SelectStep::Invalidated => SelectMarkMode::None,
            SelectStep::Ignored => st.select_mark,
        };
    });
    refresh_edit_menu();
}

/// Sync the two Begin/End Select `CheckMenuItem`s' check state and
/// sensitivity to whatever [`crate::state::GtkUiState::select_mark`]
/// currently holds. Called after every state change (on_click,
/// tab-switch reset) so the menu is always up to date without needing
/// a menu-open handler.
///
/// Sets [`REFRESHING_EDIT_MENU_MARKS`] around the `set_active` /
/// `set_sensitive` calls: the former re-emits `activate` when the
/// value flips, which would re-enter [`on_begin_end_select`]
/// synchronously (GTK 3's `gtk_check_menu_item_set_active` routes
/// through `gtk_menu_item_activate()`, not just `toggled`). The
/// handler bails while the guard is set — same shape as
/// [`refresh_view_indicators`]'s use of [`REFRESHING_MARKS`].
pub(crate) fn refresh_edit_menu() {
    let mode = with_state(|st| st.select_mark).unwrap_or(SelectMarkMode::None);
    let (stream_checked, column_checked, stream_enabled, column_enabled) = match mode {
        SelectMarkMode::None => (false, false, true, true),
        SelectMarkMode::Stream { .. } => (true, false, true, false),
        SelectMarkMode::Column { .. } => (false, true, false, true),
    };
    REFRESHING_EDIT_MENU_MARKS.with(|r| r.set(true));
    BEGIN_END_SELECT_ITEM.with(|c| {
        if let Some(item) = c.borrow().as_ref() {
            item.set_active(stream_checked);
            item.set_sensitive(stream_enabled);
        }
    });
    BEGIN_END_SELECT_COLUMN_ITEM.with(|c| {
        if let Some(item) = c.borrow().as_ref() {
            item.set_active(column_checked);
            item.set_sensitive(column_enabled);
        }
    });
    REFRESHING_EDIT_MENU_MARKS.with(|r| r.set(false));
}

#[cfg(test)]
mod begin_end_select_step_tests {
    //! Exhaustive coverage of [`resolve_begin_end_step`]. Runs on every
    //! CI runner — no GTK display, no editor widget, no `with_state` —
    //! so a regression in the state machine is caught even from the
    //! Windows runner.
    use super::{resolve_begin_end_step, SelectMarkMode, SelectStep};

    const TAB: i32 = 5;
    const OTHER_TAB: i32 = 6;
    const DOC_LEN: isize = 200;

    #[test]
    fn none_plus_stream_press_arms_stream() {
        let step = resolve_begin_end_step(SelectMarkMode::None, false, TAB, DOC_LEN);
        assert_eq!(step, SelectStep::Begin { column: false });
    }

    #[test]
    fn none_plus_column_press_arms_column() {
        let step = resolve_begin_end_step(SelectMarkMode::None, true, TAB, DOC_LEN);
        assert_eq!(step, SelectStep::Begin { column: true });
    }

    #[test]
    fn stream_armed_matching_press_ends_when_anchor_in_range() {
        let step = resolve_begin_end_step(
            SelectMarkMode::Stream {
                anchor: 100,
                tab_id: TAB,
            },
            false,
            TAB,
            DOC_LEN,
        );
        assert_eq!(
            step,
            SelectStep::End {
                anchor: 100,
                column: false
            }
        );
    }

    #[test]
    fn column_armed_matching_press_ends_when_anchor_in_range() {
        let step = resolve_begin_end_step(
            SelectMarkMode::Column {
                anchor: 42,
                tab_id: TAB,
            },
            true,
            TAB,
            DOC_LEN,
        );
        assert_eq!(
            step,
            SelectStep::End {
                anchor: 42,
                column: true
            }
        );
    }

    #[test]
    fn tab_id_mismatch_invalidates_stream() {
        // High finding: user armed on tab A, closed A, active tab is now
        // B (fresh id). Second press must NOT apply the stale anchor.
        let step = resolve_begin_end_step(
            SelectMarkMode::Stream {
                anchor: 100,
                tab_id: OTHER_TAB,
            },
            false,
            TAB,
            DOC_LEN,
        );
        assert_eq!(step, SelectStep::Invalidated);
    }

    #[test]
    fn tab_id_mismatch_invalidates_column() {
        let step = resolve_begin_end_step(
            SelectMarkMode::Column {
                anchor: 100,
                tab_id: OTHER_TAB,
            },
            true,
            TAB,
            DOC_LEN,
        );
        assert_eq!(step, SelectStep::Invalidated);
    }

    #[test]
    fn anchor_past_doc_length_invalidates() {
        // Medium finding: reload shrank the file. Same tab id, but the
        // anchor now points past EOF — a raw SCI_SETSEL(anchor, current)
        // would clamp and select "current caret to EOF".
        let step = resolve_begin_end_step(
            SelectMarkMode::Stream {
                anchor: 1000,
                tab_id: TAB,
            },
            false,
            TAB,
            500,
        );
        assert_eq!(step, SelectStep::Invalidated);
    }

    #[test]
    fn negative_anchor_invalidates() {
        // Defensive: `SCI_GETCURRENTPOS` doesn't return negatives, but
        // the type is `isize` so guard the invariant here too.
        let step = resolve_begin_end_step(
            SelectMarkMode::Stream {
                anchor: -1,
                tab_id: TAB,
            },
            false,
            TAB,
            DOC_LEN,
        );
        assert_eq!(step, SelectStep::Invalidated);
    }

    #[test]
    fn anchor_at_exactly_doc_length_is_valid() {
        // Boundary — Scintilla treats the position one past the last
        // byte as valid (that's where the caret sits at end-of-buffer).
        let step = resolve_begin_end_step(
            SelectMarkMode::Stream {
                anchor: DOC_LEN,
                tab_id: TAB,
            },
            false,
            TAB,
            DOC_LEN,
        );
        assert_eq!(
            step,
            SelectStep::End {
                anchor: DOC_LEN,
                column: false
            }
        );
    }

    #[test]
    fn stream_armed_plus_column_press_is_ignored() {
        // Sibling variant pressed while other is armed. Menu greys the
        // sibling out, but Win32 accel-table still fires WM_COMMAND —
        // must leave the arm alone.
        let step = resolve_begin_end_step(
            SelectMarkMode::Stream {
                anchor: 100,
                tab_id: TAB,
            },
            true,
            TAB,
            DOC_LEN,
        );
        assert_eq!(step, SelectStep::Ignored);
    }

    #[test]
    fn column_armed_plus_stream_press_is_ignored() {
        let step = resolve_begin_end_step(
            SelectMarkMode::Column {
                anchor: 100,
                tab_id: TAB,
            },
            false,
            TAB,
            DOC_LEN,
        );
        assert_eq!(step, SelectStep::Ignored);
    }

    // Column-arm bounds parity with the Stream side above. The
    // `(Column, true)` arm duplicates the same anchor-range guard as
    // `(Stream, false)`, so both branches need identical boundary
    // coverage — otherwise a regression on the Column-arm bounds
    // check would go uncaught.
    #[test]
    fn column_anchor_past_doc_length_invalidates() {
        let step = resolve_begin_end_step(
            SelectMarkMode::Column {
                anchor: 1000,
                tab_id: TAB,
            },
            true,
            TAB,
            500,
        );
        assert_eq!(step, SelectStep::Invalidated);
    }

    #[test]
    fn column_negative_anchor_invalidates() {
        let step = resolve_begin_end_step(
            SelectMarkMode::Column {
                anchor: -1,
                tab_id: TAB,
            },
            true,
            TAB,
            DOC_LEN,
        );
        assert_eq!(step, SelectStep::Invalidated);
    }

    #[test]
    fn column_anchor_at_exactly_doc_length_is_valid() {
        let step = resolve_begin_end_step(
            SelectMarkMode::Column {
                anchor: DOC_LEN,
                tab_id: TAB,
            },
            true,
            TAB,
            DOC_LEN,
        );
        assert_eq!(
            step,
            SelectStep::End {
                anchor: DOC_LEN,
                column: true
            }
        );
    }
}

/// Append a menu item whose right-aligned shortcut hint is arbitrary
/// text (used by Undo / Redo, which each advertise two accelerators).
/// The primary accelerator is bound with `AccelFlags::empty()` — the
/// standard `GtkAccelLabel` hint would otherwise render on top of the
/// custom label. The secondary accelerator relies on Scintilla's own
/// keymap and needs no menu-level binding.
///
/// The child is a plain `GtkBox` rather than a `GtkAccelLabel`: the
/// left `GtkLabel` handles the mnemonic (`set_use_underline` +
/// `set_mnemonic_widget` back onto the item, mirroring what
/// `MenuItem::with_mnemonic` sets up internally), and the right
/// `GtkLabel` carries the free-form hint text with the `accelerator`
/// CSS class so it picks up the same styling the sibling items get
/// from `GtkAccelLabel`.
fn append_dual_accel_edit_item(
    menu: &gtk::Menu,
    accel: &gtk::AccelGroup,
    mnemonic: &str,
    hint: &str,
    primary: (gtk::gdk::keys::Key, gtk::gdk::ModifierType),
    action: fn(),
) {
    let item = gtk::MenuItem::new();
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 24);
    let name = gtk::Label::new(None);
    name.set_use_underline(true);
    name.set_label(mnemonic);
    name.set_xalign(0.0);
    name.set_mnemonic_widget(Some(&item));
    let hint_label = gtk::Label::new(Some(hint));
    hint_label.set_xalign(1.0);
    hint_label.style_context().add_class("accelerator");
    hbox.pack_start(&name, true, true, 0);
    hbox.pack_end(&hint_label, false, false, 0);
    item.add(&hbox);
    item.connect_activate(move |_| action());
    item.add_accelerator(
        "activate",
        accel,
        *primary.0,
        primary.1,
        gtk::AccelFlags::empty(),
    );
    menu.append(&item);
}

fn build_search_menu(bar: &gtk::MenuBar, accel: &gtk::AccelGroup) {
    let none = gtk::gdk::ModifierType::empty();
    let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
    let shift = gtk::gdk::ModifierType::SHIFT_MASK;
    let entries = [
        Entry {
            label: "_Find…",
            accel: Some((key::f, ctrl)),
            action: crate::search::show_find,
        },
        Entry {
            label: "_Replace…",
            accel: Some((key::h, ctrl)),
            action: crate::search::show_replace,
        },
        Entry {
            label: "Find in _Files…",
            accel: Some((key::f, ctrl | shift)),
            action: crate::search::show_find_in_files,
        },
        Entry {
            label: "Find _Next",
            accel: Some((key::F3, none)),
            action: crate::search::find_next_repeat,
        },
        Entry {
            label: "Find _Previous",
            accel: Some((key::F3, shift)),
            action: crate::search::find_prev_repeat,
        },
        Entry {
            label: "_Go to…",
            accel: Some((key::g, ctrl)),
            action: crate::search::show_goto,
        },
    ];
    let Some(menu) = submenu_at(bar, 2, "Search") else {
        return;
    };
    populate(&menu, accel, &entries);
    menu.show_all();
}

fn build_view_menu(bar: &gtk::MenuBar, accel: &gtk::AccelGroup) {
    let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
    let zoom = [
        Entry {
            label: "Zoom _In",
            accel: Some((key::plus, ctrl)),
            action: on_zoom_in,
        },
        Entry {
            label: "Zoom _Out",
            accel: Some((key::minus, ctrl)),
            action: on_zoom_out,
        },
        Entry {
            label: "_Restore Default Zoom",
            accel: Some((key::_0, ctrl)),
            action: on_zoom_reset,
        },
    ];
    let Some(menu) = submenu_at(bar, 3, "View") else {
        return;
    };
    populate(&menu, accel, &zoom);
    // Ctrl+= is the same physical key as Ctrl++ on most layouts (+ is
    // Shift+=), so accept it for Zoom In too — matching how Win32 treats
    // VK_OEM_PLUS.
    if let Some(zoom_in) = menu.children().first() {
        zoom_in.add_accelerator(
            "activate",
            accel,
            *key::equal,
            ctrl,
            gtk::AccelFlags::VISIBLE,
        );
    }
    menu.append(&gtk::SeparatorMenuItem::new());
    // Seed the check items from the persisted View toggles. This runs while
    // the menu bar is built — *before* `restore_session` has loaded
    // session.xml into the shell — so `saved_view_settings` still returns
    // defaults here and these seeds are provisional. The authoritative
    // application to the live editor and the final check/toolbar state both
    // happen post-load in `apply_saved_view_settings` + `refresh_view_indicators`
    // (see `crate::run`), which is why nothing is pushed to the editor here.
    let view = with_state(|st| st.shell.saved_view_settings()).unwrap_or_default();
    let ww = add_check(&menu, "_Word Wrap", view.word_wrap, on_word_wrap);
    let ws = add_check(
        &menu,
        "Show White_space",
        view.show_whitespace,
        on_show_whitespace,
    );
    let eol = add_check(&menu, "Show _End of Line", view.show_eol, on_show_eol);
    // Register the checks so `refresh_view_indicators` can keep them in
    // step with the toolbar toggles, and re-sync every time the menu opens
    // (a toolbar toggle may have changed a setting since it last showed).
    VIEW_INDICATORS.with(|reg| {
        let mut reg = reg.borrow_mut();
        reg.menu_word_wrap = Some(ww);
        reg.menu_show_whitespace = Some(ws);
        reg.menu_show_eol = Some(eol);
    });

    // Folder as Workspace toggle. Unlike the three checks above it does
    // not track editor state, so it stays out of `VIEW_INDICATORS`; the
    // workspace module owns its own check↔toolbar↔panel sync (guarded
    // against the `set_active` feedback loop by `workspace::syncing`).
    menu.append(&gtk::SeparatorMenuItem::new());
    let workspace = gtk::CheckMenuItem::with_mnemonic("Folder as Works_pace");
    workspace.connect_toggled(|it| {
        if crate::workspace::syncing() {
            return;
        }
        crate::workspace::set_visible(it.is_active());
    });
    menu.append(&workspace);
    crate::workspace::register_menu_check(workspace);

    // Document Map toggle. Like the workspace toggle it tracks a panel's
    // visibility rather than an editor setting, so it owns its own
    // check↔toolbar↔panel sync (guarded against the `set_active` feedback
    // loop by `docmap::syncing`) and stays out of `VIEW_INDICATORS`.
    let docmap = gtk::CheckMenuItem::with_mnemonic("Document _Map");
    docmap.connect_toggled(|it| {
        if crate::docmap::syncing() {
            return;
        }
        crate::docmap::set_visible(it.is_active());
    });
    menu.append(&docmap);
    crate::docmap::register_menu_check(docmap);

    // Recovery action: reset the window to its default size, centred and
    // un-maximized. A plain action item (not a toggle) for when the
    // window ends up in an awkward state — e.g. dragged mostly
    // off-screen, or restored onto a monitor that has since changed.
    menu.append(&gtk::SeparatorMenuItem::new());
    let reset_window = gtk::MenuItem::with_mnemonic("Restore _Default Window Size");
    reset_window.connect_activate(|_| crate::restore_default_window_size());
    menu.append(&reset_window);

    menu.connect_show(|_| refresh_view_indicators());
    menu.show_all();
}

// --- Encoding menu ----------------------------------------------------

thread_local! {
    /// True while a menu's `show` handler is re-syncing its check marks.
    /// The programmatic `set_active` used there can re-fire an item's
    /// `activate`; the apply handlers bail when this is set so a refresh
    /// never re-applies the language/encoding it is merely *reflecting*.
    ///
    /// Deliberately shared by both the Encoding and Language menus: they
    /// run on the one GTK main thread and their show/activate sequences
    /// never interleave, so one flag is enough. A future third menu reusing
    /// it must hold that same "never concurrently refreshing" property.
    static REFRESHING_MARKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// The View-toggle indicators that must agree with the live editor:
    /// the three View-menu check items and the two toolbar toggle buttons.
    /// One [`refresh_view_indicators`] reads the editor and sets all of
    /// them, so Word Wrap and Show All Characters read the same whether the
    /// user toggled from the menu or the toolbar. Populated as the View
    /// menu and the toolbar are built; empty until then.
    static VIEW_INDICATORS: std::cell::RefCell<ViewIndicators> =
        const { std::cell::RefCell::new(ViewIndicators::new()) };
}

/// Handles to every widget that reflects a View toggle. See
/// [`VIEW_INDICATORS`].
struct ViewIndicators {
    menu_word_wrap: Option<gtk::CheckMenuItem>,
    menu_show_whitespace: Option<gtk::CheckMenuItem>,
    menu_show_eol: Option<gtk::CheckMenuItem>,
    tb_word_wrap: Option<gtk::ToggleToolButton>,
    tb_show_all_chars: Option<gtk::ToggleToolButton>,
    /// Toolbar-only — Show Indent Guide has no View-menu check (Win32
    /// parity), so this is the sole surface that reflects the setting.
    tb_indent_guide: Option<gtk::ToggleToolButton>,
}

impl ViewIndicators {
    const fn new() -> Self {
        Self {
            menu_word_wrap: None,
            menu_show_whitespace: None,
            menu_show_eol: None,
            tb_word_wrap: None,
            tb_show_all_chars: None,
            tb_indent_guide: None,
        }
    }
}

/// Register the toolbar's functional view-toggle buttons so
/// [`refresh_view_indicators`] can keep them in step with the editor (and,
/// for the first two, the menu checks). `indent_guide` has no menu check —
/// it is toolbar-only, matching Win32. Called by
/// [`crate::toolbar::build_toolbar`].
pub(crate) fn register_toolbar_view_toggles(
    word_wrap: gtk::ToggleToolButton,
    show_all_chars: gtk::ToggleToolButton,
    indent_guide: gtk::ToggleToolButton,
) {
    VIEW_INDICATORS.with(|reg| {
        let mut reg = reg.borrow_mut();
        reg.tb_word_wrap = Some(word_wrap);
        reg.tb_show_all_chars = Some(show_all_chars);
        reg.tb_indent_guide = Some(indent_guide);
    });
}

/// Set every View-toggle indicator from the live editor state, so the
/// menu checks and toolbar toggles always agree regardless of which
/// surface changed a setting.
///
/// Guarded by [`REFRESHING_MARKS`]: `set_active` re-fires an item's
/// toggled/activate signal, and the toggle handlers bail while this is set
/// so a refresh never re-applies the setting it is merely reflecting. The
/// editor is the source of truth — Show All Characters is on only when
/// both whitespace and EOL display are on, matching Win32.
pub(crate) fn refresh_view_indicators() {
    use codepp_scintilla_sys::{
        SCI_GETINDENTATIONGUIDES, SCI_GETVIEWEOL, SCI_GETVIEWWS, SCI_GETWRAPMODE, SCWS_INVISIBLE,
        SC_IV_NONE, SC_WRAP_NONE,
    };
    let Some((wrap, ws, eol, indent)) = with_state(|st| {
        let e = &st.editor;
        (
            e.send(SCI_GETWRAPMODE, 0, 0) != SC_WRAP_NONE as isize,
            e.send(SCI_GETVIEWWS, 0, 0) != SCWS_INVISIBLE as isize,
            e.send(SCI_GETVIEWEOL, 0, 0) != 0,
            e.send(SCI_GETINDENTATIONGUIDES, 0, 0) != SC_IV_NONE as isize,
        )
    }) else {
        return;
    };
    REFRESHING_MARKS.with(|r| r.set(true));
    VIEW_INDICATORS.with(|reg| {
        let reg = reg.borrow();
        if let Some(i) = &reg.menu_word_wrap {
            i.set_active(wrap);
        }
        if let Some(i) = &reg.menu_show_whitespace {
            i.set_active(ws);
        }
        if let Some(i) = &reg.menu_show_eol {
            i.set_active(eol);
        }
        if let Some(b) = &reg.tb_word_wrap {
            b.set_active(wrap);
        }
        if let Some(b) = &reg.tb_show_all_chars {
            b.set_active(ws && eol);
        }
        if let Some(b) = &reg.tb_indent_guide {
            b.set_active(indent);
        }
    });
    REFRESHING_MARKS.with(|r| r.set(false));
}

/// Build the Encoding menu: the four wired Unicode save targets plus a
/// greyed ANSI row, mirroring `ui_win32`. Selecting one flips the active
/// tab's save encoding — a metadata change realised on the next save,
/// since Scintilla always holds UTF-8 in memory — and repaints the status
/// bar. Each row is a radio-drawn `CheckMenuItem`, re-synced to the active
/// encoding every time the menu opens (`connect_show`). ANSI stays
/// disabled because `codepp_core::Encoding` has no ANSI variant yet — the
/// same reason it is greyed on Win32.
fn build_encoding_menu(bar: &gtk::MenuBar) {
    let Some(menu) = submenu_at(bar, 4, "Encoding") else {
        return;
    };
    let rows: [(&str, Option<codepp_core::Encoding>); 5] = [
        ("_ANSI", None),
        ("UTF-_8 (no BOM)", Some(codepp_core::Encoding::Utf8)),
        ("UTF-8 with _BOM", Some(codepp_core::Encoding::Utf8Bom)),
        ("UTF-16 _LE BOM", Some(codepp_core::Encoding::Utf16LeBom)),
        ("UTF-16 B_E BOM", Some(codepp_core::Encoding::Utf16BeBom)),
    ];
    let mut items: Vec<(codepp_core::Encoding, gtk::CheckMenuItem)> = Vec::new();
    for (label, enc) in rows {
        let item = gtk::CheckMenuItem::with_mnemonic(label);
        item.set_draw_as_radio(true);
        match enc {
            None => item.set_sensitive(false),
            Some(e) => {
                let apply = e.clone();
                item.connect_activate(move |_| apply_encoding(apply.clone()));
                items.push((e, item.clone()));
            }
        }
        menu.append(&item);
    }
    menu.connect_show(move |_| {
        let active = with_state(|st| st.shell.active().map(|t| t.encoding.clone())).flatten();
        set_encoding_marks(&items, active.as_ref());
    });
    menu.show_all();
}

/// Apply a chosen save encoding to the active buffer, then repaint the
/// status bar. Skips the work while a `show`-driven mark refresh is in
/// flight (see [`REFRESHING_MARKS`]).
fn apply_encoding(encoding: codepp_core::Encoding) {
    if REFRESHING_MARKS.with(std::cell::Cell::get) {
        return;
    }
    let changed = with_state(|st| st.shell.set_buffer_encoding(encoding)).unwrap_or(false);
    if changed {
        refresh_active_status();
    }
}

/// Set the encoding menu's radio marks. Both the BOM and detected-no-BOM
/// UTF-16 encodings mark the single BOM row, matching `ui_win32`; an
/// unfamiliar `Other(_)` leaves no mark (the "unknown encoding" cue).
fn set_encoding_marks(
    items: &[(codepp_core::Encoding, gtk::CheckMenuItem)],
    active: Option<&codepp_core::Encoding>,
) {
    REFRESHING_MARKS.with(|r| r.set(true));
    for (enc, item) in items {
        item.set_active(active.is_some_and(|a| same_encoding_family(a, enc)));
    }
    REFRESHING_MARKS.with(|r| r.set(false));
}

/// Whether `active` should light up the menu row for `item` — treating the
/// detected no-BOM UTF-16 variants as the same family as their BOM rows.
fn same_encoding_family(active: &codepp_core::Encoding, item: &codepp_core::Encoding) -> bool {
    use codepp_core::Encoding::{Utf16Be, Utf16BeBom, Utf16Le, Utf16LeBom, Utf8, Utf8Bom};
    matches!(
        (active, item),
        (Utf8, Utf8)
            | (Utf8Bom, Utf8Bom)
            | (Utf16LeBom | Utf16Le, Utf16LeBom)
            | (Utf16BeBom | Utf16Be, Utf16BeBom)
    )
}

// --- Language menu ----------------------------------------------------

/// Notepad++'s community UDL collection, opened by the Language menu's
/// User-Defined-language submenu. Compile-time constant — no user string
/// reaches the URI handler. Matches `ui_win32`'s `UDL_COLLECTION_URL`.
const UDL_COLLECTION_URL: &str = "https://github.com/notepad-plus-plus/userDefinedLanguages";

/// Build the Language menu from `codepp_core::lang::LANG_TABLE`, mirroring
/// Notepad++/`ui_win32`: "Normal Text" on top, a separator, then the ~88
/// languages alphabetically — a run of two or more sharing an uppercased
/// first letter collapses into a letter submenu, a lone letter stays a
/// flat item — then a separator and the "User-Defined language" submenu.
/// Each language is a radio-drawn `CheckMenuItem` whose click applies that
/// `LangType`; the active language's mark is re-synced on menu open.
fn build_language_menu(bar: &gtk::MenuBar, window: &gtk::Window) {
    let Some(menu) = submenu_at(bar, 5, "Language") else {
        return;
    };
    let table = codepp_core::lang::LANG_TABLE;
    let mut items: Vec<(i32, gtk::CheckMenuItem)> = Vec::new();

    // [0] is pinned to Normal Text — top-level, then a separator.
    if let Some(first) = table.first() {
        items.push(add_lang_item(
            &menu,
            first.menu_label,
            first.lang.as_npp_id(),
        ));
    }
    menu.append(&gtk::SeparatorMenuItem::new());

    // [1..] is alphabetical by `menu_label`; group same-first-letter runs.
    let rest = &table[1..];
    let mut i = 0;
    while i < rest.len() {
        let letter = first_letter(rest[i].menu_label);
        let mut j = i + 1;
        while j < rest.len() && first_letter(rest[j].menu_label) == letter {
            j += 1;
        }
        if j - i == 1 {
            items.push(add_lang_item(
                &menu,
                rest[i].menu_label,
                rest[i].lang.as_npp_id(),
            ));
        } else {
            let sub = gtk::Menu::new();
            for e in &rest[i..j] {
                items.push(add_lang_item(&sub, e.menu_label, e.lang.as_npp_id()));
            }
            let parent = gtk::MenuItem::with_label(&letter.to_string());
            parent.set_submenu(Some(&sub));
            menu.append(&parent);
        }
        i = j;
    }

    menu.append(&gtk::SeparatorMenuItem::new());
    menu.append(&build_udl_submenu(window));

    menu.connect_show(move |_| {
        let active = with_state(|st| st.shell.active().map(|t| t.lang.as_npp_id())).flatten();
        set_language_marks(&items, active);
    });
    menu.show_all();
}

/// Uppercased first character of a language label, for letter grouping.
/// Non-alphabetic / empty labels floor at a space, keeping them together.
fn first_letter(label: &str) -> char {
    label.chars().next().map_or(' ', |c| c.to_ascii_uppercase())
}

/// Append one language row (a radio-drawn `CheckMenuItem`) that applies
/// `lang_id` on click, and return it paired with its id for mark refresh.
/// Plain label, not mnemonic: language names carry `+`/`#`/`_` that a
/// mnemonic parse would mangle, and 88 auto-assigned mnemonics would
/// collide anyway.
fn add_lang_item(menu: &gtk::Menu, label: &str, lang_id: i32) -> (i32, gtk::CheckMenuItem) {
    let item = gtk::CheckMenuItem::with_label(label);
    item.set_draw_as_radio(true);
    item.connect_activate(move |_| apply_language(lang_id));
    menu.append(&item);
    (lang_id, item.clone())
}

/// Apply a chosen language to the active buffer: flip the tab's `lang`,
/// re-lex/re-colour via `apply_lang`, and repaint the status bar. Skips
/// the work during a `show`-driven mark refresh (see [`REFRESHING_MARKS`]).
fn apply_language(lang_id: i32) {
    if REFRESHING_MARKS.with(std::cell::Cell::get) {
        return;
    }
    let lang = codepp_core::LangType(lang_id);
    let changed = with_state(|st| st.shell.set_active_lang(lang)).unwrap_or(false);
    if changed {
        with_state(|st| {
            let (shell, mut ui) = st.split();
            ui.apply_lang(lang);
            if let Some(tab) = shell.active() {
                let (l, enc, eol, blen) = (tab.lang, tab.encoding.clone(), tab.eol, tab.byte_len);
                ui.update_status(l, &enc, eol, blen);
            }
        });
    }
}

/// Set the language menu's radio marks to the active language's id.
fn set_language_marks(items: &[(i32, gtk::CheckMenuItem)], active: Option<i32>) {
    REFRESHING_MARKS.with(|r| r.set(true));
    for (id, item) in items {
        item.set_active(active == Some(*id));
    }
    REFRESHING_MARKS.with(|r| r.set(false));
}

/// The "User-Defined language" submenu at the bottom of the Language menu.
///
/// "Define your language…" is greyed — the UDL editor modal is Phase 4.6
/// m3 and exists only on Win32 so far. The other two work: one opens the
/// `userDefineLangs` folder in the file manager, the other the N++ UDL
/// collection in the browser. Loaded UDLs are deliberately *not* listed
/// flat here yet: GTK's `apply_lang` does not style UDL buffers (it logs
/// and falls through — see `platform.rs`), so a menu entry would set a
/// language that produces no highlighting. They land when UDL styling does.
fn build_udl_submenu(window: &gtk::Window) -> gtk::MenuItem {
    let parent = gtk::MenuItem::with_label("User-Defined language");
    let sub = gtk::Menu::new();

    let define = gtk::MenuItem::with_label("Define your language…");
    define.set_sensitive(false);
    sub.append(&define);

    let open_folder = gtk::MenuItem::with_label("Open User Defined Language folder…");
    let win = window.clone();
    open_folder.connect_activate(move |_| open_udl_folder(&win));
    sub.append(&open_folder);

    let collection = gtk::MenuItem::with_label("Notepad++ User Defined Languages Collection");
    let win = window.clone();
    collection.connect_activate(move |_| open_uri(&win, UDL_COLLECTION_URL));
    sub.append(&collection);

    parent.set_submenu(Some(&sub));
    parent
}

/// Open the `userDefineLangs` folder in the desktop file manager.
/// `create_dir_all` first — matching Win32 — so a click that races a
/// between-boots deletion still targets a valid path.
fn open_udl_folder(window: &gtk::Window) {
    let Some(dir) = codepp_platform::user_define_langs_dir() else {
        tracing::warn!("no config dir; cannot open the User Defined Language folder");
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(?e, "could not create the User Defined Language folder");
        return;
    }
    match glib::filename_to_uri(&dir, None) {
        Ok(uri) => open_uri(window, &uri),
        Err(e) => tracing::warn!(?e, "filename_to_uri failed for the UDL folder"),
    }
}

/// Open `uri` with the desktop's default handler (file manager / browser).
fn open_uri(window: &gtk::Window, uri: &str) {
    if let Err(e) = gtk::show_uri_on_window(Some(window), uri, gtk::current_event_time()) {
        tracing::warn!(?e, uri, "show_uri_on_window failed");
    }
}

/// Repaint the status bar's static parts (language / EOL / encoding) from
/// the active tab — used after an encoding change, which does not re-lex.
fn refresh_active_status() {
    with_state(|st| {
        let (shell, mut ui) = st.split();
        if let Some(tab) = shell.active() {
            let (l, enc, eol, blen) = (tab.lang, tab.encoding.clone(), tab.eol, tab.byte_len);
            ui.update_status(l, &enc, eol, blen);
        }
    });
}

/// Build the Settings menu, mirroring Win32's two entries.
///
/// "Preferences…" opens the GTK Preferences dialog (the Recent Files
/// History pane — the only one wired on either backend so far). "Style
/// Configurator…" opens the GTK style editor (the Default Style + window
/// transparency, mirroring Win32's dialog scope).
fn build_settings_menu(bar: &gtk::MenuBar, window: &gtk::Window) {
    let Some(menu) = submenu_at(bar, 6, "Settings") else {
        return;
    };

    let prefs = gtk::MenuItem::with_mnemonic("_Preferences…");
    let win = window.clone();
    prefs.connect_activate(move |_| crate::preferences::show(&win));
    menu.append(&prefs);

    let style = gtk::MenuItem::with_mnemonic("_Style Configurator…");
    let win = window.clone();
    style.connect_activate(move |_| crate::style_config::show(&win));
    menu.append(&style);

    menu.show_all();
}

/// Build the ? (Help) menu, mirroring Win32's layout: the three external
/// links, then the greyed Online Manual placeholder, a separator, the
/// greyed Update placeholder, a separator, then About (F1). The greyed
/// entries hold N++-parity slots whose targets aren't wired yet.
fn build_help_menu(bar: &gtk::MenuBar, accel: &gtk::AccelGroup, window: &gtk::Window) {
    let Some(menu) = submenu_at(bar, 8, "?") else {
        return;
    };

    // Three external links — each opens a compile-time-fixed URL in the
    // desktop's default browser via `open_uri`; no user string is ever
    // passed to the URI handler.
    for (label, url) in [
        ("Code++ _Home", HELP_HOME_URL),
        ("Code++ _Project Page", HELP_PROJECT_URL),
        ("Code++ _Community (Forum)", HELP_COMMUNITY_URL),
    ] {
        let item = gtk::MenuItem::with_mnemonic(label);
        let win = window.clone();
        item.connect_activate(move |_| open_uri(&win, url));
        menu.append(&item);
    }

    // Online User Manual — greyed placeholder (no manual site yet), same as
    // Win32's `ID_HELP_MANUAL`. Sits between the links and the first
    // separator to match the Win32 order.
    let manual = gtk::MenuItem::with_mnemonic("Code++ Online User _Manual");
    manual.set_sensitive(false);
    menu.append(&manual);

    menu.append(&gtk::SeparatorMenuItem::new());

    // Update Code++ — greyed placeholder (no auto-update yet), Win32's
    // `ID_HELP_UPDATE`.
    let update = gtk::MenuItem::with_mnemonic("_Update Code++");
    update.set_sensitive(false);
    menu.append(&update);

    menu.append(&gtk::SeparatorMenuItem::new());

    // About — the one interactive item with an accelerator (F1).
    let about = gtk::MenuItem::with_mnemonic("_About Code++");
    about.connect_activate(|_| on_about());
    about.add_accelerator(
        "activate",
        accel,
        *key::F1,
        gtk::gdk::ModifierType::empty(),
        gtk::AccelFlags::VISIBLE,
    );
    menu.append(&about);

    menu.show_all();
}

/// Append each entry to `menu` as a mnemonic item bound to its
/// accelerator. Shared by every top-level menu so a label and its
/// shortcut are wired the same way everywhere.
fn populate(menu: &gtk::Menu, accel: &gtk::AccelGroup, entries: &[Entry]) {
    for e in entries {
        let item = gtk::MenuItem::with_mnemonic(e.label);
        let action = e.action;
        item.connect_activate(move |_| action());
        if let Some((key, modifier)) = e.accel {
            item.add_accelerator("activate", accel, *key, modifier, gtk::AccelFlags::VISIBLE);
        }
        menu.append(&item);
    }
}

/// Append a checkable menu item that reflects and drives a Scintilla view
/// flag, returning it so the caller can register it for refresh. `initial`
/// seeds the check to the persisted state; `toggled` receives the item's
/// new state on every user toggle.
///
/// `set_active` runs before `connect_toggled`, so seeding the restored
/// state does not fire the handler. The item is now also re-synced from
/// the editor whenever the View menu opens (see [`build_view_menu`]), so
/// it stays correct even after a toolbar toggle changed the same setting.
fn add_check(
    menu: &gtk::Menu,
    label: &str,
    initial: bool,
    toggled: fn(bool),
) -> gtk::CheckMenuItem {
    let item = gtk::CheckMenuItem::with_mnemonic(label);
    item.set_active(initial);
    item.connect_toggled(move |it| toggled(it.is_active()));
    menu.append(&item);
    item
}

pub(crate) fn on_new() {
    with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.new_untitled(&mut ui);
    });
    refresh_tab_chrome();
}

pub(crate) fn on_open() {
    // Multi-select, mirroring Win32's `OFN_ALLOWMULTISELECT` Open: the
    // user can Ctrl/Shift-click several files and they all open in one
    // dialog. Empty `Vec` on Cancel.
    open_paths(choose_open_paths());
}

/// Open every path in `paths`, in order — the shared open loop behind both
/// File → Open and drag-and-drop.
///
/// The shell dedupes already-open paths and pushes fresh tabs for the
/// rest; processing them in order leaves the view on the last file, just
/// as opening that one file alone would. There is deliberately no trailing
/// rebind after a fresh open: its async load rebinds itself when its wake
/// drains, so forcing a synchronous rebind here would paint the
/// still-empty buffer for a frame before the real content lands. An empty
/// `paths` (a cancelled dialog, or a drop that carried no local files) is
/// a no-op.
pub(crate) fn open_paths(paths: Vec<PathBuf>) {
    for path in paths {
        match with_state(|st| st.shell.open_file(path)) {
            // Already open: `Shell` moved `active_tab` with no load to
            // wake, so move the view to match. See `rebind_active_view`.
            Some(OpenFileOutcome::SwitchedToExisting(_)) => rebind_active_view(),
            // Already the active tab: nothing moved, nothing to rebind.
            Some(OpenFileOutcome::AlreadyActive) => {}
            // A load was queued; its wake drains and rebinds the view.
            // Drain anyway to flush anything already sitting in the
            // channel from an earlier iteration or operation.
            _ => drain_shell(),
        }
    }
}

/// `pub(crate)` because the close-confirm gate in `lib.rs` routes a
/// dirty buffer's Save through this same path (in place if titled, via
/// Save As if untitled), so the two never diverge.
pub(crate) fn on_save() {
    // An untitled buffer has no path to save to, so Save behaves as
    // Save As — same as Notepad++.
    let has_path = with_state(|st| st.shell.active().is_some_and(|t| t.path.is_some()));
    if has_path == Some(false) {
        on_save_as();
        return;
    }
    let result = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.save_current_to_disk(&mut ui)
    });
    if let Some(Err(err)) = result {
        // Surface, don't just log: a silent Ctrl+S failure (permission
        // denied, disk full) leaves the user believing their work is on
        // disk when it is not. Sanitized as elsewhere.
        crate::message_dialog(
            gtk::MessageType::Error,
            gtk::ButtonsType::Ok,
            "Save failed",
            &codepp_shell::sanitize_str_for_display(&err.to_string()),
        );
    }
    refresh_tab_chrome();
}

fn on_save_as() {
    let Some(path) = choose_save_path("Save As") else {
        return;
    };
    let result = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.save_buffer_as(&mut ui, path)
    });
    if let Some(Err(err)) = result {
        // Surface it rather than only logging: the standalone Save As
        // menu action and the close-confirm gate both need the user to
        // know the write did not happen. Sanitized — `ShellError`'s
        // Display can carry a path, and secondary text renders control
        // chars as real dialog lines.
        crate::message_dialog(
            gtk::MessageType::Error,
            gtk::ButtonsType::Ok,
            "Save As failed",
            &codepp_shell::sanitize_str_for_display(&err.to_string()),
        );
    }
    refresh_tab_chrome();
}

/// File → Save Session… — write every path-bound open tab to a
/// Notepad++-shape session XML at a user-picked path (distinct from the
/// automatic `session.xml` restore). Untitled buffers are skipped — they
/// have no path to record. The active tab carries its live caret + scroll
/// position; the rest record zeros, the same per-file-caret gap the
/// automatic session save documents. The intricate build lives in the
/// shared `Shell::save_npp_session`; this only snapshots the caret and
/// picks the path.
fn on_save_session() {
    use codepp_scintilla_sys::{
        SCI_GETFIRSTVISIBLELINE, SCI_GETSELECTIONEND, SCI_GETSELECTIONSTART,
    };
    let Some(path) = choose_save_path("Save Session") else {
        return;
    };
    // Snapshot the caret and build + write under one borrow taken *after*
    // the dialog closes, so the recorded caret and the tab list it attaches
    // to are a single consistent snapshot — an async load or watcher event
    // during the chooser's nested loop can't split them apart. The caret is
    // captured only when the active tab is path-bound (an untitled active
    // tab contributes no `<File>` entry).
    let result = with_state(|st| {
        let active_caret = st
            .shell
            .active()
            .is_some_and(|t| t.path.is_some())
            .then(|| codepp_shell::SessionCaret {
                start_pos: st.editor.send(SCI_GETSELECTIONSTART, 0, 0).max(0) as u64,
                end_pos: st.editor.send(SCI_GETSELECTIONEND, 0, 0).max(0) as u64,
                first_visible_line: st.editor.send(SCI_GETFIRSTVISIBLELINE, 0, 0).max(0) as u32,
            });
        st.shell.save_npp_session(&path, active_caret)
    });
    if let Some(Err(err)) = result {
        crate::message_dialog(
            gtk::MessageType::Error,
            gtk::ButtonsType::Ok,
            "Save Session failed",
            &codepp_shell::sanitize_str_for_display(&err.to_string()),
        );
    }
}

/// File → Load Session… — open the tabs recorded in a Notepad++-shape
/// session XML at a user-picked path. The shared `Shell::load_npp_session`
/// filters empty-name / over-cap entries, pre-seeds each tab's
/// caret/lang/pin metadata, opens the files, and restores the recorded
/// active tab; the async loads land on their own wakes, so a final
/// `drain_shell` flushes anything already queued and `rebind_active_view`
/// moves the view onto the resolved active tab.
///
/// Unlike Win32 this does not first discard a sole empty "new 1" scratch
/// buffer — a leftover empty tab alongside the loaded session is cosmetic,
/// and GTK's tab layer always keeps at least one tab, so there is no
/// null-state hazard. Tracked as a follow-up for exact Win32 parity.
fn on_load_session() {
    let Some(path) = choose_open_paths().into_iter().next() else {
        return;
    };
    let report = with_state(|st| st.shell.load_npp_session(&path));
    match report {
        Some(Ok(r)) => {
            // A session that was *entirely* rejected opened nothing; say
            // so rather than let it vanish silently. (Non-local paths are
            // only rejected on Windows; on Linux this count is always 0.)
            if r.opened == 0 && r.rejected_nonlocal > 0 {
                crate::message_dialog(
                    gtk::MessageType::Info,
                    gtk::ButtonsType::Ok,
                    "Load Session",
                    &format!(
                        "This session file contained {} network / UNC path(s), \
                         which Code++ does not open from session files. \
                         No local files were opened.",
                        r.rejected_nonlocal
                    ),
                );
            }
            if r.dropped_over_cap > 0 {
                tracing::warn!(
                    cap = codepp_shell::MAX_SESSION_TABS,
                    dropped = r.dropped_over_cap,
                    "Load Session: entries exceed cap; excess dropped",
                );
            }
        }
        Some(Err(err)) => {
            crate::message_dialog(
                gtk::MessageType::Error,
                gtk::ButtonsType::Ok,
                "Load Session failed",
                &codepp_shell::sanitize_str_for_display(&err.to_string()),
            );
            return;
        }
        None => return,
    }
    rebind_active_view();
    drain_shell();
}

fn on_reload() {
    with_state(|st| st.shell.reload_active());
    drain_shell();
}

pub(crate) fn on_close() {
    close_active_tab();
}

pub(crate) fn on_save_all() {
    let errors = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.save_all(&mut ui)
    })
    .unwrap_or_default();
    refresh_tab_chrome();
    if errors.is_empty() {
        return;
    }
    // List the failures by buffer name. `tab_display_name` sanitizes the
    // name and the error text is sanitized here; the `\n` joiners are
    // ours, added after sanitization (which would otherwise strip them).
    let body = with_state(|st| {
        errors
            .iter()
            .map(|(id, err)| {
                let name = st
                    .shell
                    .tabs
                    .iter()
                    .find(|t| t.id == *id)
                    .map_or_else(|| format!("buffer {id}"), codepp_shell::tab_display_name);
                format!(
                    "{name}: {}",
                    codepp_shell::sanitize_str_for_display(&err.to_string())
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
    .unwrap_or_default();
    crate::message_dialog(
        gtk::MessageType::Error,
        gtk::ButtonsType::Ok,
        "Save All — some files were not saved",
        &body,
    );
}

pub(crate) fn on_close_all() {
    // Loop the single-tab close so each dirty buffer gets its own
    // Save / Don't Save / Cancel prompt and a Cancel stops the rest —
    // matching Win32's `close_multiple_documents`. `close_active_tab`
    // returns `false` when the user aborts.
    loop {
        let before = with_state(|st| st.shell.tabs.len()).unwrap_or(0);
        if before == 0 {
            break;
        }
        if !close_active_tab() {
            break;
        }
        // Defensive: never spin if a close somehow made no progress.
        if with_state(|st| st.shell.tabs.len()).unwrap_or(0) >= before {
            break;
        }
    }
}

/// Send a parameterless command to the active Scintilla view. The `SCI_*`
/// edit and zoom commands all take this shape.
fn editor_cmd(msg: u32) {
    with_state(|st| {
        st.editor.send(msg, 0, 0);
    });
}

pub(crate) fn on_undo() {
    editor_cmd(codepp_scintilla_sys::SCI_UNDO);
    refresh_tab_chrome();
}

pub(crate) fn on_redo() {
    editor_cmd(codepp_scintilla_sys::SCI_REDO);
    refresh_tab_chrome();
}

pub(crate) fn on_cut() {
    editor_cmd(codepp_scintilla_sys::SCI_CUT);
    refresh_tab_chrome();
}

pub(crate) fn on_copy() {
    editor_cmd(codepp_scintilla_sys::SCI_COPY);
}

pub(crate) fn on_paste() {
    editor_cmd(codepp_scintilla_sys::SCI_PASTE);
    refresh_tab_chrome();
}

fn on_delete() {
    editor_cmd(codepp_scintilla_sys::SCI_CLEAR);
    refresh_tab_chrome();
}

fn on_select_all() {
    editor_cmd(codepp_scintilla_sys::SCI_SELECTALL);
}

pub(crate) fn on_zoom_in() {
    editor_cmd(codepp_scintilla_sys::SCI_ZOOMIN);
}

pub(crate) fn on_zoom_out() {
    editor_cmd(codepp_scintilla_sys::SCI_ZOOMOUT);
}

fn on_zoom_reset() {
    with_state(|st| {
        st.editor.send(codepp_scintilla_sys::SCI_SETZOOM, 0, 0);
    });
}

/// Push the four GTK-exposed view toggles into the live editor. Shared
/// by cold-start restore ([`build_view_menu`]) and — via each handler's
/// read-modify-write — every user toggle, so the editor and `Shell`'s
/// persisted copy never disagree.
fn apply_view_settings(
    editor: &codepp_editor::EditorHandle,
    view: codepp_core::session::ViewSettings,
) {
    let wrap = if view.word_wrap {
        codepp_scintilla_sys::SC_WRAP_WORD
    } else {
        codepp_scintilla_sys::SC_WRAP_NONE
    };
    let ws = if view.show_whitespace {
        codepp_scintilla_sys::SCWS_VISIBLEALWAYS
    } else {
        codepp_scintilla_sys::SCWS_INVISIBLE
    };
    // Indent guides: `SC_IV_LOOKBOTH` (guides drawn through blank lines too)
    // when on, `SC_IV_NONE` when off — the exact pair Win32's toggle uses.
    // The guide colour (`STYLE_INDENTGUIDE`) is seeded once at startup by
    // `apply_indent_guide_style`, so flipping the mode alone makes it appear.
    editor.send(
        codepp_scintilla_sys::SCI_SETINDENTATIONGUIDES,
        if view.indent_guide {
            codepp_scintilla_sys::SC_IV_LOOKBOTH
        } else {
            codepp_scintilla_sys::SC_IV_NONE
        },
        0,
    );
    editor.send(codepp_scintilla_sys::SCI_SETWRAPMODE, wrap, 0);
    // Clear the tracking-mode horizontal scroll high-water mark
    // (`view.lineWidthMaxSeen`) so it recomputes for the current — possibly
    // newly-unwrapped — content. That mark never shrinks on its own and is
    // shared across the single view, so a long line seen earlier (or the
    // full-view-width wrapped layout itself) would otherwise leave a
    // phantom horizontal scroll into empty space every time wrap is toggled
    // back off. `SCI_SETSCROLLWIDTH(1)` zeroes it; the re-layout re-measures
    // the visible lines. See the setup in `crate::run`.
    //
    // This also fires for the whitespace / EOL toggles, which share this
    // function — harmless there: it just forces a same-content re-measure.
    editor.send(codepp_scintilla_sys::SCI_SETSCROLLWIDTH, 1, 0);
    editor.send(codepp_scintilla_sys::SCI_SETVIEWWS, ws, 0);
    editor.send(
        codepp_scintilla_sys::SCI_SETVIEWEOL,
        usize::from(view.show_eol),
        0,
    );
}

/// Push the persisted View settings onto the live editor at cold start,
/// then sync every indicator (menu checks + toolbar toggles) to match.
///
/// Called from [`crate::run`] **after** `restore_session` has loaded
/// session.xml into the shell — the point at which `saved_view_settings`
/// finally returns the user's stored choices rather than defaults. This is
/// the GTK analogue of Win32's `apply_saved_view_settings`. Without it the
/// editor keeps Scintilla's built-in off-defaults, and the first user
/// toggle would resurface every stored setting at once via
/// [`toggle_view_setting`]'s full re-apply — the "toggling indent guide also
/// turned word wrap on after a restart" bug.
///
/// Safe to run before `restore_session`'s async `OpenFile` loads land: wrap
/// mode, whitespace, EOL and indent guides are Scintilla *view* properties,
/// not per-document, so the `SCI_SETDOCPOINTER` rebinds those loads perform
/// later do not reset them — one application to the single view covers every
/// tab.
pub(crate) fn apply_saved_view_settings() {
    with_state(|st| {
        let view = st.shell.saved_view_settings();
        apply_view_settings(&st.editor, view);
    });
    refresh_view_indicators();
}

/// Mutate the persisted View settings with `f`, apply them to the editor,
/// then re-sync every indicator. Bails while a refresh is in flight (see
/// [`refresh_view_indicators`]) so a programmatic `set_active` cannot
/// re-apply the setting it is only reflecting. Shared by every View
/// toggle, from either the menu or the toolbar.
pub(crate) fn toggle_view_setting(f: impl FnOnce(&mut codepp_core::session::ViewSettings)) {
    if REFRESHING_MARKS.with(std::cell::Cell::get) {
        return;
    }
    with_state(|st| {
        let mut view = st.shell.saved_view_settings();
        f(&mut view);
        apply_view_settings(&st.editor, view);
        // Persist so the choice survives to the next session save.
        st.shell.set_view_settings(view);
    });
    refresh_view_indicators();
}

pub(crate) fn on_word_wrap(active: bool) {
    toggle_view_setting(|v| v.word_wrap = active);
}

fn on_show_whitespace(active: bool) {
    toggle_view_setting(|v| v.show_whitespace = active);
}

fn on_show_eol(active: bool) {
    toggle_view_setting(|v| v.show_eol = active);
}

/// The toolbar's "Show All Characters" toggle — whitespace *and* EOL
/// together, matching Win32's combined button.
pub(crate) fn on_show_all_chars(active: bool) {
    toggle_view_setting(|v| {
        v.show_whitespace = active;
        v.show_eol = active;
    });
}

/// The toolbar's "Show Indent Guide" toggle — the vertical guide lines
/// drawn through leading whitespace. Toolbar-only, matching Win32 (there is
/// no View-menu counterpart on either backend). Persisted on `ViewSettings`
/// so the choice survives to the next launch, exactly like the siblings.
pub(crate) fn on_indent_guide(active: bool) {
    toggle_view_setting(|v| v.indent_guide = active);
}

/// Code++ home page, the About dialog's website link and the ? →
/// "Code++ Home" entry. Mirrors `ui_win32`'s `HELP_HOME_URL`; the two
/// backends must agree.
const HELP_HOME_URL: &str = "https://code-plus-plus.org/";
/// ? → "Code++ Project Page". Mirrors `ui_win32`'s `HELP_PROJECT_URL`.
const HELP_PROJECT_URL: &str = "https://github.com/TheFlipside/code-plus-plus";
/// ? → "Code++ Community (Forum)". Mirrors `ui_win32`'s `HELP_COMMUNITY_URL`.
const HELP_COMMUNITY_URL: &str = "https://community.code-plus-plus.org/";

fn on_about() {
    let parent = with_state(|st| st.window.clone());
    let dialog = gtk::AboutDialog::new();
    dialog.set_program_name("Code++");
    dialog.set_version(Some(env!("CARGO_PKG_VERSION")));
    dialog.set_comments(Some(
        "A fast, cross-platform code and text editor built on Scintilla.",
    ));
    dialog.set_website(Some(HELP_HOME_URL));
    dialog.set_website_label(Some("code-plus-plus.org"));
    dialog.set_license_type(gtk::License::MitX11);
    if let Some(parent) = parent.as_ref() {
        dialog.set_transient_for(Some(parent));
    }
    dialog.set_modal(true);
    dialog.run();
    // SAFETY: created here and never handed out — see `message_dialog`.
    unsafe {
        dialog.destroy();
    }
}

/// Run a native Open chooser with multi-selection enabled and return
/// every path the user picked (empty on Cancel).
///
/// The GTK counterpart of Win32's
/// [`prompt_open_paths`](../../ui_win32/index.html) — `set_select_multiple(true)`
/// is the `OFN_ALLOWMULTISELECT` analogue, and `filenames()` returns the
/// whole selection already decoded to `PathBuf`s, so there is no
/// double-NUL buffer to parse. Save stays single-select via
/// [`choose_save_path`].
fn choose_open_paths() -> Vec<PathBuf> {
    let parent = with_state(|st| st.window.clone());
    let chooser = gtk::FileChooserNative::new(
        Some("Open"),
        parent.as_ref(),
        gtk::FileChooserAction::Open,
        Some("_Open"),
        Some("_Cancel"),
    );
    chooser.set_select_multiple(true);
    let paths = if chooser.run() == gtk::ResponseType::Accept {
        chooser.filenames()
    } else {
        Vec::new()
    };
    // `FileChooserNative` keeps the dialog alive until destroyed
    // explicitly; without this a cancelled chooser leaks its window.
    chooser.destroy();
    paths
}

/// Run a native Save chooser and return the chosen path (None on Cancel).
///
/// `FileChooserNative` rather than `FileChooserDialog` so the dialog is
/// the desktop's own — the GTK counterpart of Win32's
/// `GetSaveFileNameW`, and what a portal-based desktop expects. Open is a
/// separate function ([`choose_open_paths`]) because it is multi-select
/// and returns a `Vec`; keeping this save-only avoids a dead Open branch.
fn choose_save_path(title: &str) -> Option<PathBuf> {
    let parent = with_state(|st| st.window.clone());
    let chooser = gtk::FileChooserNative::new(
        Some(title),
        parent.as_ref(),
        gtk::FileChooserAction::Save,
        Some("_Save"),
        Some("_Cancel"),
    );
    // Offer to overwrite rather than silently clobbering.
    chooser.set_do_overwrite_confirmation(true);
    let path = if chooser.run() == gtk::ResponseType::Accept {
        chooser.filename()
    } else {
        None
    };
    // `FileChooserNative` keeps the dialog alive until it is destroyed
    // explicitly; without this a cancelled chooser leaks its window.
    chooser.destroy();
    path
}
