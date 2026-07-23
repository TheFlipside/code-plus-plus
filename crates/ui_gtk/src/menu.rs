//! The menu bar and its handlers.
//!
//! Wired: File (New, Open, Save, Save As, Save All, Reload, Close, Close
//! All, Exit), Edit (Undo/Redo, Cut/Copy/Paste/Delete, Select All),
//! Search (Find, Replace, Find Next/Previous, Go to), View (zoom, Word
//! Wrap, Show Whitespace, Show EOL) and ? (About). Still to come, tracked
//! against the Win32 parity list: Encoding, Language, Settings, Tools,
//! Macro, Run, Plugins and Window.
//!
//! Accelerators match the Win32 backend's `CreateAcceleratorTableW`
//! block, which DESIGN.md §7.5 names as the source of truth for
//! hotkeys across all three platforms.

use std::path::PathBuf;

use codepp_shell::OpenFileOutcome;
use gtk::gdk::keys::constants as key;
use gtk::prelude::*;

use crate::state::with_state;
use crate::{
    close_active_tab, drain_shell, rebind_active_view, refresh_tab_chrome, save_session_now,
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

/// Build the menu bar. Handlers are attached separately by [`connect`],
/// because they need the window state installed first.
pub fn build() -> gtk::MenuBar {
    let bar = gtk::MenuBar::new();
    // Order mirrors Notepad++/Win32: File, Edit, Search, View, … , ?.
    // "?" is N++'s Help menu; kept as-is for parity.
    for title in ["_File", "_Edit", "_Search", "_View", "?"] {
        let root = gtk::MenuItem::with_mnemonic(title);
        root.set_submenu(Some(&gtk::Menu::new()));
        bar.append(&root);
    }
    bar
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
    build_help_menu(&bar, &accel);
}

fn build_file_menu(bar: &gtk::MenuBar, accel: &gtk::AccelGroup) {
    let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
    let ctrl_shift = ctrl | gtk::gdk::ModifierType::SHIFT_MASK;
    // MOD1 is Alt. Save As is Ctrl+Alt+S (Win32 parity), which frees
    // Ctrl+Shift+S for Save All.
    let ctrl_alt = ctrl | gtk::gdk::ModifierType::MOD1_MASK;
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
            label: "_Reload",
            accel: Some((key::r, ctrl)),
            action: on_reload,
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
    menu.append(&gtk::SeparatorMenuItem::new());
    let exit = gtk::MenuItem::with_mnemonic("E_xit");
    exit.connect_activate(|_| {
        save_session_now();
        gtk::main_quit();
    });
    menu.append(&exit);
    menu.show_all();
}

/// The Edit menu — Win32's minimal Scintilla-backed set. Delete carries
/// no application accelerator: the Del key stays with Scintilla for
/// normal forward-delete; the menu item just exposes `SCI_CLEAR`.
fn build_edit_menu(bar: &gtk::MenuBar, accel: &gtk::AccelGroup) {
    let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
    let undo = [
        Entry {
            label: "_Undo",
            accel: Some((key::z, ctrl)),
            action: on_undo,
        },
        Entry {
            label: "_Redo",
            accel: Some((key::y, ctrl)),
            action: on_redo,
        },
    ];
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
    ];
    let select = [Entry {
        label: "Select _All",
        accel: Some((key::a, ctrl)),
        action: on_select_all,
    }];
    let Some(menu) = submenu_at(bar, 1, "Edit") else {
        return;
    };
    populate(&menu, accel, &undo);
    menu.append(&gtk::SeparatorMenuItem::new());
    populate(&menu, accel, &clip);
    menu.append(&gtk::SeparatorMenuItem::new());
    populate(&menu, accel, &select);
    menu.show_all();
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
    // Restore the persisted View toggles: apply them to the live editor
    // and seed the check items so the menu and the view agree from the
    // first frame — the cold-start restore Win32 does at
    // `apply_saved_view_settings` time. Read once here; the toggle
    // handlers keep `Shell`'s copy updated from now on.
    let view = with_state(|st| {
        let view = st.shell.saved_view_settings();
        apply_view_settings(&st.editor, view);
        view
    })
    .unwrap_or_default();
    add_check(&menu, "_Word Wrap", view.word_wrap, on_word_wrap);
    add_check(
        &menu,
        "Show White_space",
        view.show_whitespace,
        on_show_whitespace,
    );
    add_check(&menu, "Show _End of Line", view.show_eol, on_show_eol);
    menu.show_all();
}

fn build_help_menu(bar: &gtk::MenuBar, accel: &gtk::AccelGroup) {
    let none = gtk::gdk::ModifierType::empty();
    let entries = [Entry {
        label: "_About Code++",
        accel: Some((key::F1, none)),
        action: on_about,
    }];
    let Some(menu) = submenu_at(bar, 4, "?") else {
        return;
    };
    populate(&menu, accel, &entries);
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
/// flag. `initial` seeds the check to the persisted state; `toggled`
/// receives the item's new state on every user toggle.
///
/// `set_active` runs before `connect_toggled`, so seeding the restored
/// state does not fire the handler. No refresh-on-popup is needed either:
/// nothing outside these items changes wrap / whitespace / EOL
/// visibility, so the check *is* the state once seeded.
fn add_check(menu: &gtk::Menu, label: &str, initial: bool, toggled: fn(bool)) {
    let item = gtk::CheckMenuItem::with_mnemonic(label);
    item.set_active(initial);
    item.connect_toggled(move |it| toggled(it.is_active()));
    menu.append(&item);
}

fn on_new() {
    with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.new_untitled(&mut ui);
    });
    refresh_tab_chrome();
}

fn on_open() {
    // Multi-select, mirroring Win32's `OFN_ALLOWMULTISELECT` Open: the
    // user can Ctrl/Shift-click several files and they all open in one
    // dialog. Empty `Vec` on Cancel.
    let paths = choose_open_paths();
    if paths.is_empty() {
        return;
    }
    // Run the exact single-open handling once per picked path. The shell
    // dedupes already-open paths and pushes fresh tabs for the rest;
    // processing them in order leaves the view on the last file, just as
    // picking that one file alone would. There is deliberately no trailing
    // rebind: a fresh open's async load rebinds itself when its wake
    // drains, so forcing a synchronous rebind here would paint the
    // still-empty buffer for a frame before the real content lands.
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

fn on_reload() {
    with_state(|st| st.shell.reload_active());
    drain_shell();
}

fn on_close() {
    close_active_tab();
}

fn on_save_all() {
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

fn on_close_all() {
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

fn on_undo() {
    editor_cmd(codepp_scintilla_sys::SCI_UNDO);
    refresh_tab_chrome();
}

fn on_redo() {
    editor_cmd(codepp_scintilla_sys::SCI_REDO);
    refresh_tab_chrome();
}

fn on_cut() {
    editor_cmd(codepp_scintilla_sys::SCI_CUT);
    refresh_tab_chrome();
}

fn on_copy() {
    editor_cmd(codepp_scintilla_sys::SCI_COPY);
}

fn on_paste() {
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

fn on_zoom_in() {
    editor_cmd(codepp_scintilla_sys::SCI_ZOOMIN);
}

fn on_zoom_out() {
    editor_cmd(codepp_scintilla_sys::SCI_ZOOMOUT);
}

fn on_zoom_reset() {
    with_state(|st| {
        st.editor.send(codepp_scintilla_sys::SCI_SETZOOM, 0, 0);
    });
}

/// Push the three GTK-exposed view toggles into the live editor. Shared
/// by cold-start restore ([`build_view_menu`]) and — via each handler's
/// read-modify-write — every user toggle, so the editor and `Shell`'s
/// persisted copy never disagree. `indent_guide` is in `ViewSettings`
/// too, but GTK exposes no toggle for it, so it is left alone here.
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
    editor.send(codepp_scintilla_sys::SCI_SETWRAPMODE, wrap, 0);
    editor.send(codepp_scintilla_sys::SCI_SETVIEWWS, ws, 0);
    editor.send(
        codepp_scintilla_sys::SCI_SETVIEWEOL,
        usize::from(view.show_eol),
        0,
    );
}

fn on_word_wrap(active: bool) {
    with_state(|st| {
        let mut view = st.shell.saved_view_settings();
        view.word_wrap = active;
        apply_view_settings(&st.editor, view);
        // Persist so the choice survives to the next session save.
        st.shell.set_view_settings(view);
    });
}

fn on_show_whitespace(active: bool) {
    with_state(|st| {
        let mut view = st.shell.saved_view_settings();
        view.show_whitespace = active;
        apply_view_settings(&st.editor, view);
        st.shell.set_view_settings(view);
    });
}

fn on_show_eol(active: bool) {
    with_state(|st| {
        let mut view = st.shell.saved_view_settings();
        view.show_eol = active;
        apply_view_settings(&st.editor, view);
        st.shell.set_view_settings(view);
    });
}

/// Code++ home page, the About dialog's website link. Mirrors
/// `ui_win32`'s `HELP_HOME_URL`; the two backends must agree.
const HELP_HOME_URL: &str = "https://code-plus-plus.org/";

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
