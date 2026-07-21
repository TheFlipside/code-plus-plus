//! The menu bar and its handlers.
//!
//! m2 wires File only, and only the entries whose `Shell` methods
//! already exist: New, Open, Save, Save As, Reload, Close, Exit. The
//! full Notepad++ menu set (Edit, Search, View, Encoding, Language,
//! Settings, Tools, Macro, Run, Plugins, Window, ?) lands alongside the
//! tab strip and the dialogs in a later milestone.
//!
//! Accelerators match the Win32 backend's `CreateAcceleratorTableW`
//! block, which DESIGN.md §7.5 names as the source of truth for
//! hotkeys across all three platforms.

use std::path::PathBuf;

use codepp_shell::OpenFileOutcome;
use gtk::prelude::*;

use crate::state::with_state;
use crate::{close_active_tab, drain_shell, rebind_active_view, refresh_title, save_session_now};

/// Menu item labels paired with the accelerator each one advertises.
/// Kept next to the handler wiring so a label and its shortcut cannot
/// drift apart.
struct Entry {
    label: &'static str,
    key: gtk::gdk::keys::Key,
    modifier: gtk::gdk::ModifierType,
    action: fn(),
}

/// Build the menu bar. Handlers are attached separately by [`connect`],
/// because they need the window state installed first.
pub fn build() -> gtk::MenuBar {
    let bar = gtk::MenuBar::new();
    let file_menu = gtk::Menu::new();
    let file_root = gtk::MenuItem::with_mnemonic("_File");
    file_root.set_submenu(Some(&file_menu));
    bar.append(&file_root);
    bar
}

/// Populate the File menu and bind its accelerators.
///
/// Split from [`build`] so the window is fully constructed and the
/// state installed before any handler can possibly fire.
pub fn connect() {
    let Some((bar, window)) = with_state(|st| (st.menu_bar.clone(), st.window.clone())) else {
        return;
    };
    let accel = gtk::AccelGroup::new();
    window.add_accel_group(&accel);

    let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
    let ctrl_shift = ctrl | gtk::gdk::ModifierType::SHIFT_MASK;
    let entries = [
        Entry {
            label: "_New",
            key: gtk::gdk::keys::constants::n,
            modifier: ctrl,
            action: on_new,
        },
        Entry {
            label: "_Open…",
            key: gtk::gdk::keys::constants::o,
            modifier: ctrl,
            action: on_open,
        },
        Entry {
            label: "_Save",
            key: gtk::gdk::keys::constants::s,
            modifier: ctrl,
            action: on_save,
        },
        Entry {
            label: "Save _As…",
            key: gtk::gdk::keys::constants::S,
            modifier: ctrl_shift,
            action: on_save_as,
        },
        Entry {
            label: "_Reload",
            key: gtk::gdk::keys::constants::r,
            modifier: ctrl,
            action: on_reload,
        },
        Entry {
            label: "_Close",
            key: gtk::gdk::keys::constants::w,
            modifier: ctrl,
            action: on_close,
        },
    ];

    // `build` left the File submenu empty so this function owns the
    // whole population step; fetch it back rather than threading it
    // through the state struct.
    let Some(file_root) = bar
        .children()
        .first()
        .and_then(|c| c.clone().downcast::<gtk::MenuItem>().ok())
    else {
        tracing::error!("menu bar has no File item; menu not wired");
        return;
    };
    let Some(file_menu) = file_root
        .submenu()
        .and_then(|m| m.downcast::<gtk::Menu>().ok())
    else {
        tracing::error!("File item has no submenu; menu not wired");
        return;
    };

    for e in entries {
        let item = gtk::MenuItem::with_mnemonic(e.label);
        let action = e.action;
        item.connect_activate(move |_| action());
        item.add_accelerator(
            "activate",
            &accel,
            *e.key,
            e.modifier,
            gtk::AccelFlags::VISIBLE,
        );
        file_menu.append(&item);
    }

    file_menu.append(&gtk::SeparatorMenuItem::new());
    let exit = gtk::MenuItem::with_mnemonic("E_xit");
    exit.connect_activate(|_| {
        save_session_now();
        gtk::main_quit();
    });
    file_menu.append(&exit);
    file_menu.show_all();
}

fn on_new() {
    with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.new_untitled(&mut ui);
    });
    refresh_title();
}

fn on_open() {
    let Some(path) = choose_file(gtk::FileChooserAction::Open, "Open") else {
        return;
    };
    let outcome = with_state(|st| st.shell.open_file(path));
    match outcome {
        // The path was already open, so no load runs and no wake will
        // fire — `Shell` moved `active_tab` and it is on us to move the
        // view to match. See `rebind_active_view` for what goes wrong
        // otherwise.
        Some(OpenFileOutcome::SwitchedToExisting(_)) => rebind_active_view(),
        // Already the active tab: nothing moved, nothing to rebind.
        Some(OpenFileOutcome::AlreadyActive) => {}
        // A load was queued. The worker's wake drains it and
        // `apply_load_result` rebinds the view, so there is nothing to
        // do synchronously — drain anyway to flush anything already
        // sitting in the channel from an earlier operation.
        _ => drain_shell(),
    }
}

fn on_save() {
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
        tracing::warn!(%err, "save failed");
    }
    refresh_title();
}

fn on_save_as() {
    let Some(path) = choose_file(gtk::FileChooserAction::Save, "Save As") else {
        return;
    };
    let result = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.save_buffer_as(&mut ui, path)
    });
    if let Some(Err(err)) = result {
        tracing::warn!(%err, "save-as failed");
    }
    refresh_title();
}

fn on_reload() {
    with_state(|st| st.shell.reload_active());
    drain_shell();
}

fn on_close() {
    close_active_tab();
}

/// Run a native file chooser and return the chosen path.
///
/// `FileChooserNative` rather than `FileChooserDialog` so the dialog is
/// the desktop's own — the GTK counterpart of Win32's
/// `GetOpenFileNameW`/`GetSaveFileNameW`, and what a portal-based
/// desktop expects.
fn choose_file(action: gtk::FileChooserAction, title: &str) -> Option<PathBuf> {
    let parent = with_state(|st| st.window.clone());
    let accept = if action == gtk::FileChooserAction::Save {
        "_Save"
    } else {
        "_Open"
    };
    let chooser = gtk::FileChooserNative::new(
        Some(title),
        parent.as_ref(),
        action,
        Some(accept),
        Some("_Cancel"),
    );
    // Offer to overwrite rather than silently clobbering.
    chooser.set_do_overwrite_confirmation(action == gtk::FileChooserAction::Save);
    let response = chooser.run();
    let path = if response == gtk::ResponseType::Accept {
        chooser.filename()
    } else {
        None
    };
    // `FileChooserNative` keeps the dialog alive until it is destroyed
    // explicitly; without this a cancelled chooser leaks its window.
    chooser.destroy();
    path
}
