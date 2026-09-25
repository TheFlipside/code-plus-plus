//! Settings → Preferences… dialog for the GTK backend.
//!
//! Mirrors the Win32 `preferences` module, layout included: a category
//! list on the left and the selected category's page on the right. Two
//! categories today, in the Win32 list's order and with its names —
//! **Recent Files History**
//! ([`codepp_core::preferences::RecentFilesHistoryConfig`]) and
//! **Security** ([`codepp_core::preferences::SecurityConfig`]), the guard
//! on plugin panels' startup commands. The pages sit in a `GtkStack`,
//! and the list is a `GtkListBox` in a bordered scrolled window, like the
//! Win32 list box, with a row made from each of the stack's pages, so a
//! page cannot lack a row or a row a page. The dialog opens with keyboard
//! focus in the list, as the Win32 one does, so the arrow keys pick a
//! category straight away. (`GtkStackSidebar` would bind a list to the
//! stack by itself, but themes style it as a sidebar with an edge of its
//! own, which doubles the border.)
//!
//! A page the stack is not showing keeps its controls, so Close reads
//! every page back, not only the one on screen, and writes the result
//! through [`codepp_shell::Shell`]'s `set_preferences`, which clamps and
//! persists it. The File menu's Recent Files region picks up the change
//! the next time it opens, and the guard is consulted at the next start.
//!
//! Control labels and frame captions match the Win32 pages exactly,
//! including the negative-sense "Don't check at launch time" checkbox.

use codepp_core::preferences::{
    Preferences, RecentFileDisplayMode, RecentFilesHistoryConfig, CUSTOM_MAX_LENGTH_LIMIT,
    MAX_ENTRIES_LIMIT,
};
use gtk::prelude::*;

use crate::state::with_state;

/// The dialog's initial size, in pixels: the Win32 dialog's client area,
/// so the list and the pages keep its proportions. GTK makes it larger
/// if a theme's fonts need more room.
const DIALOG_W: i32 = 720;
const DIALOG_H: i32 = 440;

/// Width of the category list, the Win32 list box's.
const LIST_W: i32 = 180;

/// The Win32 dialog's padding, used here as the margin around the body,
/// the gap between the list and the page, and the gap between a page's
/// frames.
const PAD: i32 = 12;

/// The Security page's checkbox: what the switch does, in the Win32
/// pane's words.
const VERIFY_PANEL_COMMANDS_LABEL: &str = "Run only signed plugin panel commands at startup";

/// What the checkbox means, below it — the Win32 pane's text, with how
/// this platform keeps the key in place of DPAPI (see
/// `codepp_platform::panel_key`).
const VERIFY_PANEL_COMMANDS_HELP: &str = "\
Code++ reopens a plugin's panel at startup the way Notepad++ does: \
by running the plugin's own command for it. It signs each command it \
records from the panel's own plugin, with a key kept in your Code++ \
settings folder that no other account can read.\n\n\
With this on, Code++ neither runs a command it did not sign nor loads \
its plugin at startup. That covers a command from an edited or copied \
session file, and one set by a plugin that is not the panel's own. \
The panel keeps its place until you open it from its plugin's menu.\n\n\
With this off, every saved command runs, as in Notepad++.";

/// The Recent Files History page's controls.
struct HistoryControls {
    /// "Don't check at launch time". Negative sense: checked means the
    /// feature is off.
    dont_check: gtk::CheckButton,
    max_entries: gtk::SpinButton,
    in_submenu: gtk::CheckButton,
    only_name: gtk::RadioButton,
    custom: gtk::RadioButton,
    custom_length: gtk::SpinButton,
}

/// Every control [`read_back`] reads, from every page. Held so
/// [`build_content`] can build the pages while [`read_back`] later reads
/// them, keeping [`show`] short.
struct Controls {
    history: HistoryControls,
    /// The Security page's checkbox.
    verify: gtk::CheckButton,
}

/// What [`build_content`] builds.
struct Body {
    /// The category list beside the page stack: the dialog's content.
    root: gtk::Box,
    /// The category list, whose selected row has keyboard focus when the
    /// dialog opens.
    list: gtk::ListBox,
    /// Every page's controls, for [`read_back`].
    controls: Controls,
}

/// Show the modal Preferences dialog. Reads the current preferences,
/// presents them a category at a time, and on Close writes the (clamped)
/// result back through `Shell::set_preferences`.
pub(crate) fn show(window: &gtk::Window) {
    let Some(current) = with_state(|st| st.shell.preferences.clone()) else {
        return;
    };
    let (dialog, controls) = build_dialog(window, &current);
    run_frozen(&dialog);
    // Every close — the button, Esc, the window's own close — reads back,
    // as on Win32.
    let updated = read_back(&controls, &current);

    // SAFETY: built by `build_dialog` for this call and never handed out —
    // same idiom as the other GTK modals (About, Plugin Manager).
    unsafe {
        dialog.destroy();
    }

    // Persist only on an actual change (the next Recent-Files-menu open
    // reflects it, and the next start's restore reads the guard).
    // `set_preferences` clamps and writes through to the config file.
    if updated != current {
        with_state(|st| st.shell.set_preferences(updated));
    }
    // Unfrozen now: apply anything a worker completed while the modal
    // held the main loop.
    crate::drain_shell();
}

/// Build the dialog over `window`, seeded from `current`, and show it with
/// keyboard focus on the category list's selected row. Returns the dialog
/// and the [`Controls`] [`read_back`] reads.
fn build_dialog(window: &gtk::Window, current: &Preferences) -> (gtk::Dialog, Controls) {
    let dialog = gtk::Dialog::with_buttons(
        Some("Preferences"),
        Some(window),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[("_Close", gtk::ResponseType::Close)],
    );
    dialog.set_default_size(DIALOG_W, DIALOG_H);
    let content = dialog.content_area();
    content.set_margin_top(PAD);
    content.set_margin_bottom(PAD);
    content.set_margin_start(PAD);
    content.set_margin_end(PAD);
    let body = build_content(current);
    content.pack_start(&body.root, true, true, 0);

    dialog.show_all();
    // The Win32 dialog sets focus on its list when it opens. GTK would
    // pick the first focusable widget, which is the list only while
    // nothing focusable is packed ahead of it, so set it here too. The
    // dialog is already showing, so `run` does not show it again and
    // leaves the focus where this puts it.
    if let Some(row) = body.list.selected_row() {
        row.grab_focus();
    }
    (dialog, body.controls)
}

/// Run `dialog`'s modal loop with the drain frozen. `run` spins a nested
/// main loop, where a worker's wake would drain the shell under the
/// modal — the Plugin Manager's reason too. Nothing the dialog reads back
/// comes from live shell state, so today the worst case would be a stale
/// value; frozen, it stays that way as the dialog grows. The caller
/// flushes once the dialog is gone.
fn run_frozen(dialog: &gtk::Dialog) {
    let _freeze = crate::DrainFreeze::new();
    dialog.run();
}

/// Build the dialog's content, seeded from `current`: the category list
/// beside a stack holding one page per category.
fn build_content(current: &Preferences) -> Body {
    let (recent_files, history) = recent_files_page(&current.recent_files_history);
    let (security, verify) = security_page(current.security.verify_panel_commands);

    // Titled and ordered as the Win32 list's rows: `category_list` makes
    // its rows from these. Each page is shown before it is added, because
    // a stack never shows a hidden child. A page's name in the stack is its
    // title, since nothing looks a page up by name.
    let stack = gtk::Stack::new();
    for (page, title) in [
        (&recent_files, "Recent Files History"),
        (&security, "Security"),
    ] {
        page.show_all();
        stack.add_titled(page, title, title);
    }

    let (scroll, list) = category_list(&stack);
    let root = gtk::Box::new(gtk::Orientation::Horizontal, PAD);
    root.pack_start(&scroll, false, false, 0);
    root.pack_start(&stack, true, true, 0);
    Body {
        root,
        list,
        controls: Controls { history, verify },
    }
}

/// The category list: a row for each of `stack`'s pages, in order and
/// showing the page's title, and picking a row shows its page. Opens on
/// the first row, as the Win32 list does. Returns the list in its
/// bordered scrolled window, and the list itself.
fn category_list(stack: &gtk::Stack) -> (gtk::ScrolledWindow, gtk::ListBox) {
    let list = gtk::ListBox::new();
    // Browse: a row is always selected, and the arrow keys move the
    // selection, as in the Win32 list box.
    list.set_selection_mode(gtk::SelectionMode::Browse);
    for page in stack.children() {
        let label = gtk::Label::new(stack.child_title(&page).as_deref());
        label.set_xalign(0.0);
        label.set_margin_top(4);
        label.set_margin_bottom(4);
        label.set_margin_start(6);
        label.set_margin_end(6);
        list.add(&label);
    }
    let pages = stack.clone();
    list.connect_row_selected(move |_, row| {
        crate::at_callback_boundary("preferences:category:selected", (), || {
            // The rows were made in page order, and a stack keeps its
            // pages in the order they were added, so a row's index is its
            // page's.
            let page = row
                .and_then(|r| usize::try_from(r.index()).ok())
                .and_then(|i| pages.children().get(i).cloned());
            if let Some(page) = page {
                pages.set_visible_child(&page);
            }
        });
    });
    list.select_row(list.row_at_index(0).as_ref());

    // The scrolled window draws the border; the viewport draws none of its
    // own, whatever the theme's default.
    let viewport = gtk::Viewport::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    viewport.set_shadow_type(gtk::ShadowType::None);
    viewport.add(&list);
    let scroll = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_shadow_type(gtk::ShadowType::In);
    scroll.set_size_request(LIST_W, -1);
    scroll.add(&viewport);
    (scroll, list)
}

/// The Recent Files History page: the Win32 page's two frames, "Recent
/// Files History" and "Display", seeded from `cfg`. Returns the page and
/// its controls.
fn recent_files_page(cfg: &RecentFilesHistoryConfig) -> (gtk::Box, HistoryControls) {
    let history = section_box();
    // Negative-sense checkbox: checked means the feature is OFF
    // (`enabled == false`), matching N++/Win32's "Don't check at launch
    // time" wording. `read_back` inverts it symmetrically.
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
    let max_entries = gtk::SpinButton::with_range(0.0, f64::from(MAX_ENTRIES_LIMIT), 1.0);
    max_entries.set_value(f64::from(cfg.max_entries));
    max_row.pack_start(&max_entries, false, false, 0);
    max_row.pack_start(
        &gtk::Label::new(Some(&format!("(0 - {MAX_ENTRIES_LIMIT})"))),
        false,
        false,
        0,
    );
    history.pack_start(&max_row, false, false, 0);

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
    let custom_length = gtk::SpinButton::with_range(1.0, f64::from(CUSTOM_MAX_LENGTH_LIMIT), 1.0);
    custom_length.set_value(f64::from(cfg.custom_max_length));
    custom_row.pack_start(&custom_length, false, false, 0);
    custom_row.pack_start(
        &gtk::Label::new(Some(&format!("(1 - {CUSTOM_MAX_LENGTH_LIMIT})"))),
        false,
        false,
        0,
    );
    display.pack_start(&custom_row, false, false, 0);

    // Seed the radio group from the stored display mode.
    match cfg.display_mode {
        RecentFileDisplayMode::OnlyFileName => only_name.set_active(true),
        RecentFileDisplayMode::FullPath => full_path.set_active(true),
        RecentFileDisplayMode::CustomMaxLength => custom.set_active(true),
    }
    // The custom-length field is only meaningful for the "Customize" mode;
    // grey it out otherwise, and keep that in step as the radio changes.
    custom_length.set_sensitive(custom.is_active());
    let spin = custom_length.clone();
    custom.connect_toggled(move |r| {
        crate::at_callback_boundary("preferences:custom:toggled", (), || {
            spin.set_sensitive(r.is_active());
        });
    });

    let page = page_box();
    page.pack_start(&framed("Recent Files History", &history), false, false, 0);
    page.pack_start(&framed("Display", &display), false, false, 0);
    let controls = HistoryControls {
        dont_check,
        max_entries,
        in_submenu,
        only_name,
        custom,
        custom_length,
    };
    (page, controls)
}

/// The Security page: the startup-command guard's checkbox, seeded with
/// `verify`, and what it means below it, in a frame captioned as the Win32
/// page's is. Returns the page and the checkbox.
fn security_page(verify: bool) -> (gtk::Box, gtk::CheckButton) {
    let section = section_box();
    let check = gtk::CheckButton::with_label(VERIFY_PANEL_COMMANDS_LABEL);
    check.set_active(verify);
    section.pack_start(&check, false, false, 0);
    let help = gtk::Label::new(Some(VERIFY_PANEL_COMMANDS_HELP));
    help.set_xalign(0.0);
    help.set_line_wrap(true);
    help.set_max_width_chars(60);
    help.style_context().add_class("dim-label");
    section.pack_start(&help, false, false, 0);

    let page = page_box();
    page.pack_start(&framed("Plugin panels", &section), false, false, 0);
    (page, check)
}

/// Read every page's controls back into a copy of `prior`, whichever page
/// is showing. `SpinButton` ranges already clamp, and
/// `Shell::set_preferences` clamps again defensively, so `as u32` here is
/// never lossy for a value the widget could produce.
fn read_back(c: &Controls, prior: &Preferences) -> Preferences {
    let h = &c.history;
    // A spin button takes a typed value only when it loses focus, and
    // Esc or the window's own close leaves it with focus: take the value
    // now, as the Win32 dialog reads its edit fields' text on every close.
    h.max_entries.update();
    h.custom_length.update();

    let mut updated = prior.clone();
    let out = &mut updated.recent_files_history;
    // Inverts the seeding in `recent_files_page`. The two must stay in
    // step: otherwise opening and closing the dialog flips the feature,
    // and turning it off clears the recent-files list.
    out.enabled = !h.dont_check.is_active();
    out.in_submenu = h.in_submenu.is_active();
    out.max_entries = h.max_entries.value_as_int().max(0) as u32;
    out.custom_max_length = h.custom_length.value_as_int().max(1) as u32;
    out.display_mode = if h.only_name.is_active() {
        RecentFileDisplayMode::OnlyFileName
    } else if h.custom.is_active() {
        RecentFileDisplayMode::CustomMaxLength
    } else {
        RecentFileDisplayMode::FullPath
    };
    updated.security.verify_panel_commands = c.verify.is_active();
    updated
}

/// A page of the stack: its frames, stacked from the top as the Win32
/// page lays them out.
fn page_box() -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, PAD)
}

/// A vertical box with the padding shared by the pages' framed sections.
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

/// The category list, the dialog's focus and the read-back. Display-gated,
/// because they build real widgets: driven by `crate::display_tests`,
/// which owns the invocation and explains why these cannot be `#[test]`s
/// of their own.
#[cfg(test)]
pub(crate) mod dialog_tests {
    use super::{build_content, build_dialog, read_back, Body};
    use codepp_core::preferences::{
        Preferences, RecentFileDisplayMode, CUSTOM_MAX_LENGTH_LIMIT, MAX_ENTRIES_LIMIT,
    };
    use gtk::prelude::*;

    /// The list has the Win32 list's rows, in its order, and each row shows
    /// the page holding its own controls. The dialog opens on the first.
    pub(crate) fn the_category_list_shows_the_page_it_names() {
        gtk::init().expect("gtk::init failed — no display?");
        let body = build_content(&Preferences::default());
        body.root.show_all();
        let stack = stack_of(&body);
        let list = &body.list;

        let titles: Vec<String> = list.children().iter().map(row_title).collect();
        assert_eq!(titles, ["Recent Files History", "Security"]);

        let history = body.controls.history.dont_check.upcast_ref::<gtk::Widget>();
        let guard = body.controls.verify.upcast_ref::<gtk::Widget>();
        assert_eq!(list.selected_row().map(|r| r.index()), Some(0));
        assert!(showing(&stack, history), "opens on Recent Files History");
        // Back to the first row as well, so its page is shown by the row and
        // not only by being where the dialog opens.
        select(list, 1);
        assert!(showing(&stack, guard), "the Security row shows the guard");
        select(list, 0);
        assert!(showing(&stack, history), "the first row shows its page");
    }

    /// The dialog opens with keyboard focus on the list's selected row,
    /// where the Win32 dialog puts it, so the arrow keys pick a category
    /// straight away.
    pub(crate) fn the_dialog_opens_with_focus_in_the_category_list() {
        gtk::init().expect("gtk::init failed — no display?");
        let parent = gtk::Window::new(gtk::WindowType::Toplevel);
        let (dialog, _) = build_dialog(&parent, &Preferences::default());
        let list: gtk::ListBox =
            find(dialog.upcast_ref()).expect("the dialog holds a category list");
        let row = list.selected_row().expect("a row is selected");
        assert_eq!(dialog.focused_widget(), Some(row.upcast()));
        // SAFETY: both built for this test and never handed out.
        unsafe {
            dialog.destroy();
            parent.destroy();
        }
    }

    /// Closing an untouched dialog changes nothing, whatever was stored:
    /// the negative-sense checkbox is seeded and read back inverted, and
    /// the two must cancel. And every page is read back as the user left
    /// it, the one not showing included, down to a number typed into a
    /// spin button and never committed, which is what Esc leaves behind.
    pub(crate) fn read_back_takes_every_page_as_the_user_left_it() {
        gtk::init().expect("gtk::init failed — no display?");
        for prefs in samples() {
            let body = build_content(&prefs);
            body.root.show_all();
            assert_eq!(
                read_back(&body.controls, &prefs),
                prefs,
                "an untouched dialog changes nothing"
            );
        }

        let prior = Preferences::default();
        let body = build_content(&prior);
        body.root.show_all();
        let stack = stack_of(&body);
        let (c, list) = (&body.controls, &body.list);
        // Changed on the Security page, which is then left.
        select(list, 1);
        c.verify.set_active(!prior.security.verify_panel_commands);
        select(list, 0);
        assert!(
            !showing(&stack, c.verify.upcast_ref()),
            "the Security page must not be showing"
        );
        let h = &c.history;
        h.dont_check.set_active(prior.recent_files_history.enabled);
        h.in_submenu
            .set_active(!prior.recent_files_history.in_submenu);
        h.custom.set_active(true);
        h.custom_length.set_value(42.0);
        h.max_entries.set_text("7");
        assert_ne!(
            h.max_entries.value_as_int(),
            7,
            "the typed value must still be uncommitted"
        );

        let got = read_back(c, &prior);
        let out = &got.recent_files_history;
        assert_eq!(out.enabled, !prior.recent_files_history.enabled);
        assert_eq!(out.in_submenu, !prior.recent_files_history.in_submenu);
        assert_eq!(out.display_mode, RecentFileDisplayMode::CustomMaxLength);
        assert_eq!(out.custom_max_length, 42);
        assert_eq!(out.max_entries, 7);
        assert_eq!(
            got.security.verify_panel_commands,
            !prior.security.verify_panel_commands
        );
    }

    /// Stored preferences covering each display mode, each switch both
    /// ways, and both ends of each number's range.
    fn samples() -> Vec<Preferences> {
        let mut flipped = Preferences::default();
        let h = &mut flipped.recent_files_history;
        h.enabled = !h.enabled;
        h.in_submenu = !h.in_submenu;
        h.display_mode = RecentFileDisplayMode::OnlyFileName;
        h.max_entries = 0;
        h.custom_max_length = 1;
        flipped.security.verify_panel_commands = !flipped.security.verify_panel_commands;

        let mut limits = Preferences::default();
        let h = &mut limits.recent_files_history;
        h.display_mode = RecentFileDisplayMode::CustomMaxLength;
        h.max_entries = MAX_ENTRIES_LIMIT;
        h.custom_max_length = CUSTOM_MAX_LENGTH_LIMIT;

        let default = Preferences::default();
        assert_eq!(
            default.recent_files_history.display_mode,
            RecentFileDisplayMode::FullPath,
            "the third display mode is covered by the default"
        );
        vec![default, flipped, limits]
    }

    /// The page stack in `body`.
    fn stack_of(body: &Body) -> gtk::Stack {
        find(body.root.upcast_ref()).expect("the body holds a page stack")
    }

    /// Whether `widget` is on the page `stack` is showing.
    fn showing(stack: &gtk::Stack, widget: &gtk::Widget) -> bool {
        stack
            .visible_child()
            .is_some_and(|page| widget.is_ancestor(&page))
    }

    /// The first widget of type `T` at or below `widget`, depth first.
    fn find<T: IsA<gtk::Widget>>(widget: &gtk::Widget) -> Option<T> {
        if let Ok(found) = widget.clone().downcast::<T>() {
            return Some(found);
        }
        widget
            .downcast_ref::<gtk::Container>()?
            .children()
            .iter()
            .find_map(find::<T>)
    }

    /// What a row of the category list shows.
    fn row_title(row: &gtk::Widget) -> String {
        row.downcast_ref::<gtk::ListBoxRow>()
            .and_then(gtk::prelude::BinExt::child)
            .and_then(|label| label.downcast::<gtk::Label>().ok())
            .expect("a row holds a label")
            .text()
            .into()
    }

    /// Pick row `i`, as a click or an arrow key does.
    fn select(list: &gtk::ListBox, i: usize) {
        let row = list
            .row_at_index(i32::try_from(i).expect("a small index"))
            .expect("the row exists");
        list.select_row(Some(&row));
    }
}
