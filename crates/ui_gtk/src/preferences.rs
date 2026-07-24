//! Settings → Preferences… dialog for the GTK backend.
//!
//! Mirrors the Win32 `preferences` module. It edits the one pane wired on
//! either backend so far — **Recent Files History**
//! ([`codepp_core::preferences::RecentFilesHistoryConfig`]). On Close the
//! controls are read back and written through [`codepp_shell::Shell`]'s
//! `set_preferences`, which clamps and persists them; the File menu's
//! Recent Files submenu picks up the change the next time it opens.
//!
//! The Win32 dialog is a category-list + panel design; with a single
//! category a plain framed modal is the honest GTK shape, and it can grow
//! into a sidebar when more panes land. Control labels and the negative-sense
//! "Don't check at launch time" checkbox match the Win32 pane exactly.

use codepp_core::preferences::{RecentFileDisplayMode, CUSTOM_MAX_LENGTH_LIMIT, MAX_ENTRIES_LIMIT};
use gtk::prelude::*;

use crate::state::with_state;

/// Show the modal Preferences dialog. Reads the current preferences,
/// presents the Recent Files History controls, and on Close writes the
/// (clamped) result back through `Shell::set_preferences`.
pub(crate) fn show(window: &gtk::Window) {
    let Some(current) = with_state(|st| st.shell.preferences.clone()) else {
        return;
    };
    let cfg = &current.recent_files_history;

    let dialog = gtk::Dialog::with_buttons(
        Some("Preferences"),
        Some(window),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[("_Close", gtk::ResponseType::Close)],
    );
    dialog.set_default_size(420, -1);
    let content = dialog.content_area();
    content.set_spacing(10);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(10);
    content.set_margin_end(10);

    // --- Recent Files History frame ---
    let history = section_box();
    // Negative-sense checkbox: checked means the feature is OFF
    // (`enabled == false`), matching N++/Win32's "Don't check at launch
    // time" wording. The read-back below inverts it symmetrically.
    let dont_check = gtk::CheckButton::with_label("Don't check at launch time");
    dont_check.set_active(!cfg.enabled);
    history.pack_start(&dont_check, false, false, 0);

    let max_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    max_row.pack_start(
        &gtk::Label::new(Some("Max. number of entries:")),
        false,
        false,
        0,
    );
    let max_spin = gtk::SpinButton::with_range(0.0, f64::from(MAX_ENTRIES_LIMIT), 1.0);
    max_spin.set_value(f64::from(cfg.max_entries));
    max_row.pack_start(&max_spin, false, false, 0);
    max_row.pack_start(
        &gtk::Label::new(Some(&format!("(0 - {MAX_ENTRIES_LIMIT})"))),
        false,
        false,
        0,
    );
    history.pack_start(&max_row, false, false, 0);
    content.pack_start(&framed("Recent Files History", &history), false, false, 0);

    // --- Display frame ---
    let display = section_box();
    let in_submenu = gtk::CheckButton::with_label("In Submenu");
    in_submenu.set_active(cfg.in_submenu);
    display.pack_start(&in_submenu, false, false, 0);

    let only_name = gtk::RadioButton::with_label("Only File Name");
    let full_path = gtk::RadioButton::with_label_from_widget(&only_name, "Full File Name Path");
    let custom = gtk::RadioButton::with_label_from_widget(&only_name, "Customize Maximum Length:");
    display.pack_start(&only_name, false, false, 0);
    display.pack_start(&full_path, false, false, 0);

    let custom_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    custom_row.pack_start(&custom, false, false, 0);
    let custom_spin = gtk::SpinButton::with_range(1.0, f64::from(CUSTOM_MAX_LENGTH_LIMIT), 1.0);
    custom_spin.set_value(f64::from(cfg.custom_max_length));
    custom_row.pack_start(&custom_spin, false, false, 0);
    custom_row.pack_start(
        &gtk::Label::new(Some(&format!("(1 - {CUSTOM_MAX_LENGTH_LIMIT})"))),
        false,
        false,
        0,
    );
    display.pack_start(&custom_row, false, false, 0);
    content.pack_start(&framed("Display", &display), false, false, 0);

    // Seed the radio group from the stored display mode.
    match cfg.display_mode {
        RecentFileDisplayMode::OnlyFileName => only_name.set_active(true),
        RecentFileDisplayMode::FullPath => full_path.set_active(true),
        RecentFileDisplayMode::CustomMaxLength => custom.set_active(true),
    }
    // The custom-length field is only meaningful for the "Customize" mode;
    // grey it out otherwise, and keep that in step as the radio changes.
    custom_spin.set_sensitive(custom.is_active());
    let spin = custom_spin.clone();
    custom.connect_toggled(move |r| spin.set_sensitive(r.is_active()));

    dialog.show_all();
    dialog.run();

    // Read the controls back. `SpinButton` ranges already clamp, and
    // `Shell::set_preferences` clamps again defensively, so `as u32` here is
    // never lossy for a value the widget could produce.
    let mut updated = current.clone();
    {
        let out = &mut updated.recent_files_history;
        out.enabled = !dont_check.is_active();
        out.in_submenu = in_submenu.is_active();
        out.max_entries = max_spin.value_as_int().max(0) as u32;
        out.custom_max_length = custom_spin.value_as_int().max(1) as u32;
        out.display_mode = if only_name.is_active() {
            RecentFileDisplayMode::OnlyFileName
        } else if custom.is_active() {
            RecentFileDisplayMode::CustomMaxLength
        } else {
            RecentFileDisplayMode::FullPath
        };
    }

    // SAFETY: created here and never handed out — same idiom as the other
    // GTK modals (About, Plugin Manager).
    unsafe {
        dialog.destroy();
    }

    // Persist only on an actual change (the next Recent-Files-menu open
    // reflects it). `set_preferences` clamps and writes through to the
    // config file.
    if updated != current {
        with_state(|st| st.shell.set_preferences(updated));
    }
}

/// A vertical box with the padding shared by the dialog's framed sections.
fn section_box() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    b.set_margin_top(6);
    b.set_margin_bottom(6);
    b.set_margin_start(8);
    b.set_margin_end(8);
    b
}

/// Wrap `child` in a titled `GtkFrame`.
fn framed(title: &str, child: &impl IsA<gtk::Widget>) -> gtk::Frame {
    let frame = gtk::Frame::new(Some(title));
    frame.add(child);
    frame
}
