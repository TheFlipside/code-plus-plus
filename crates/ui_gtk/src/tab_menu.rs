//! The tab strip's context menu on GTK.
//!
//! A right click on a tab brings it to the front, through the same switch a
//! left click makes ([`crate::select_tab_by_id`]), delivers what the switch
//! queued, and then pops up the menu `codepp_shell` builds for it
//! ([`codepp_shell::Shell::tab_context_menu`]), as on Win32
//! (`ui_win32::tab_menu`). Most entries run the File-menu command they stand
//! for; the rest are carried out here.
//!
//! # Why the menu waits for an idle
//!
//! The press lands in the tab label's event box, inside the notebook. The
//! switch rebuilds the strip's labels, which drops the very event box whose
//! handler is running, and delivering the switch runs plugin code. So the
//! handler only records the press and stops it there
//! ([`request`]); the switch, the delivery and the popup run from a
//! high-priority idle once GTK has finished with the event, the reason
//! `crate::deliver_after_signal` gives for the notebook's own signals.
//!
//! # Every entry is for the tab it was built for
//!
//! The menu records the tab's buffer id, and each entry checks that the tab
//! is still in front when it is chosen ([`crate::active_tab_is`]). A worker's
//! result applied while the menu was up can move the front; an entry then
//! does nothing rather than act on another tab.

use std::cell::RefCell;

use codepp_shell::{ClipboardPart, TabColor, TabCommand, TabMenuItem};
use gtk::glib;
use gtk::prelude::*;

use crate::state::with_state;

thread_local! {
    /// The press waiting for [`show`]: the tab's buffer id and the press.
    /// The popup is placed by the press's root coordinates, against the
    /// main window: the event's own window is the label's event box, which
    /// the switch to the tab destroys before the popup is placed. The
    /// event is still handed to GTK as the popup's trigger, for its device
    /// and its time, which the menu's grab needs and nothing reads the
    /// window of. A second press before the idle runs replaces it, and the
    /// idle is not queued twice.
    static PENDING: RefCell<Option<(i32, gtk::gdk::EventButton)>> = const { RefCell::new(None) };
}

/// Side of the square drawn beside each Apply Color entry, in pixels.
const SWATCH_PX: i32 = 14;

/// Record a right click on the tab with buffer id `id` (`press`), and
/// show its menu from a high-priority idle (see the module docs). Nothing
/// is queued once a quit has been asked for, or while a nested loop holds
/// the drain's freeze (a modal prompt is up), when a switch and its
/// delivery must not run.
pub(crate) fn request(id: i32, press: gtk::gdk::EventButton) {
    if crate::quit_requested() || crate::DrainFreeze::active() {
        PENDING.with(|p| p.borrow_mut().take());
        return;
    }
    if PENDING.with(|p| p.replace(Some((id, press)))).is_some() {
        return;
    }
    glib::idle_add_local_full(glib::Priority::HIGH, || {
        crate::at_callback_boundary("tab_menu:show", glib::ControlFlow::Break, || {
            if let Some((id, press)) = PENDING.with(|p| p.borrow_mut().take()) {
                show(id, &press);
            }
            glib::ControlFlow::Break
        })
    });
}

/// Bring the tab `id` to the front and pop up its menu where `press` was.
fn show(id: i32, press: &gtk::gdk::EventButton) {
    if crate::quit_requested() || crate::DrainFreeze::active() {
        return;
    }
    if !crate::active_tab_is(id) {
        crate::select_tab_by_id(id);
        // The switch is announced like a left click's, before the menu.
        crate::drain_shell();
    }
    // The switch did not happen, or delivering it moved the front: never
    // show one tab's menu for another.
    if !crate::active_tab_is(id) {
        return;
    }
    let Some(Some((items, window))) = with_state(|st| {
        let idx = st.shell.active_tab?;
        let dirty = crate::active_holds_unsaved(st);
        Some((st.shell.tab_context_menu(idx, dirty), st.window.clone()))
    }) else {
        return;
    };
    // Placed against the main window, which outlives the switch, at the
    // point the press was made.
    let root = press.root();
    let Some(gdk_window) = window.window() else {
        return;
    };
    let (_, origin_x, origin_y) = gdk_window.origin();
    let at = gtk::gdk::Rectangle::new(
        clamp_px(root.0) - origin_x,
        clamp_px(root.1) - origin_y,
        1,
        1,
    );
    let popup = build_menu(&items, id);
    popup.set_attach_widget(Some(&window));
    // Destroyed once the menu is done with: `selection-done` comes after
    // the chosen entry's `activate` has run, and also when the menu is
    // dismissed. A popup menu sits in a toplevel GTK keeps a reference
    // to, so it is never freed by dropping it. From an idle, so the
    // destroy never runs inside the menu's own emission.
    popup.connect_selection_done(|popup| {
        crate::at_callback_boundary("tab_menu:selection_done", (), || {
            let popup = popup.clone();
            glib::idle_add_local_once(move || {
                crate::at_callback_boundary("tab_menu:destroy", (), || {
                    tracing::debug!("tab menu destroyed");
                    // SAFETY: the menu was built by `show` for this one
                    // popup and is handed to nothing else; no Scintilla
                    // view is in it.
                    unsafe { popup.destroy() };
                });
            });
        });
    });
    popup.show_all();
    popup.popup_at_rect(
        &gdk_window,
        &at,
        gtk::gdk::Gravity::SouthEast,
        gtk::gdk::Gravity::NorthWest,
        Some(&**press),
    );
}

/// A root coordinate as a whole pixel, saturating.
#[allow(clippy::cast_possible_truncation)]
fn clamp_px(v: f64) -> i32 {
    // `as` saturates for out-of-range floats and maps NaN to 0.
    v.round() as i32
}

/// A menu holding `items`, whose entries act on the tab `id`.
fn build_menu(items: &[TabMenuItem], id: i32) -> gtk::Menu {
    let menu = gtk::Menu::new();
    for item in items {
        match item {
            TabMenuItem::Separator => menu.append(&gtk::SeparatorMenuItem::new()),
            TabMenuItem::Command {
                label,
                command,
                enabled,
                swatch,
            } => {
                let entry = match swatch {
                    Some(color) => swatch_item(label, *color),
                    None => gtk::MenuItem::with_label(label),
                };
                entry.set_sensitive(*enabled);
                let command = *command;
                entry.connect_activate(move |_| {
                    crate::at_callback_boundary("tab_menu:item:activate", (), || run(id, command));
                });
                menu.append(&entry);
            }
            TabMenuItem::Submenu {
                label,
                enabled,
                items,
            } => {
                let entry = gtk::MenuItem::with_label(label);
                entry.set_submenu(Some(&build_menu(items, id)));
                entry.set_sensitive(*enabled);
                menu.append(&entry);
            }
        }
    }
    menu
}

/// An entry with the square of `color` left of `label`.
fn swatch_item(label: &str, color: TabColor) -> gtk::MenuItem {
    let entry = gtk::MenuItem::new();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let square = gtk::DrawingArea::new();
    square.set_size_request(SWATCH_PX, SWATCH_PX);
    square.set_valign(gtk::Align::Center);
    let (red, green, blue) = rgb_f64(color.bar_rgb());
    square.connect_draw(move |area, cr| {
        crate::at_callback_boundary("tab_menu:swatch:draw", glib::Propagation::Proceed, || {
            let width = f64::from(area.allocated_width());
            let height = f64::from(area.allocated_height());
            // `fill` fails only when the cairo context is already in an
            // error state; a square left unpainted is all that costs.
            // A grey border, so the yellow reads on a light theme.
            cr.set_source_rgb(0.376, 0.376, 0.376);
            cr.rectangle(0.0, 0.0, width, height);
            let _ = cr.fill();
            cr.set_source_rgb(red, green, blue);
            cr.rectangle(1.0, 1.0, width - 2.0, height - 2.0);
            let _ = cr.fill();
            glib::Propagation::Proceed
        })
    });
    row.pack_start(&square, false, false, 0);
    row.pack_start(&gtk::Label::new(Some(label)), false, false, 0);
    entry.add(&row);
    entry
}

/// `0xRRGGBB` as cairo's three components.
pub(crate) fn rgb_f64(rgb: u32) -> (f64, f64, f64) {
    let c = |shift: u32| f64::from((rgb >> shift) & 0xFF) / 255.0;
    (c(16), c(8), c(0))
}

/// Carry out `command` for the tab `id`, if it is still in front.
fn run(id: i32, command: TabCommand) {
    use crate::menu::ContainingAction;
    use TabCommand as C;
    if !crate::active_tab_is(id) {
        tracing::debug!(
            id,
            ?command,
            "the tab left the front while its menu was up; nothing done"
        );
        return;
    }
    match command {
        C::Close => crate::menu::on_close(),
        C::CloseMultiple(kind) => crate::close_multiple_documents(kind),
        C::TogglePin => crate::toggle_pin_by_id(id),
        C::Save => crate::menu::on_save(),
        C::SaveAs => crate::menu::on_save_as(),
        C::OpenContainingFileManager => {
            crate::menu::open_containing(ContainingAction::FileManager);
        }
        C::OpenContainingTerminal => crate::menu::open_containing(ContainingAction::Terminal),
        C::OpenContainingWorkspace => crate::menu::open_containing(ContainingAction::Workspace),
        C::OpenInDefaultViewer => crate::menu::on_open_in_default_viewer(),
        C::Rename => crate::menu::on_rename(),
        C::MoveToRecycleBin => crate::menu::on_move_to_recycle_bin(),
        C::Reload => crate::menu::on_reload(),
        C::Print => crate::print::show(),
        C::CopyFullPath => copy(ClipboardPart::FullPath),
        C::CopyFileName => copy(ClipboardPart::FileName),
        C::CopyDirPath => copy(ClipboardPart::DirPath),
        C::MoveToStart => move_to_edge(false),
        C::MoveToEnd => move_to_edge(true),
        C::MoveToNewInstance => new_instance(id, true),
        C::OpenInNewInstance => new_instance(id, false),
        C::ApplyColor(color) => apply_color(Some(color)),
        C::RemoveColor => apply_color(None),
        // PowerShell is offered on Windows only, and the other views wait
        // for a second view: none of these entries can be chosen here.
        C::OpenContainingPowerShell | C::MoveToOtherView | C::CloneToOtherView => {}
    }
}

/// Copy `part` of the active tab to the clipboard.
fn copy(part: ClipboardPart) {
    let text = with_state(|st| {
        let idx = st.shell.active_tab?;
        st.shell.tab_clipboard_text(idx, part)
    })
    .flatten();
    if let Some(text) = text {
        let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
        clipboard.set_text(&text);
        // Hand the text to the clipboard manager, if one runs, so it
        // survives Code++ exiting.
        clipboard.store();
    }
}

/// Move the active tab to the start or the end of the strip, and deliver
/// the `NPPN_DOCORDERCHANGED` it queued.
fn move_to_edge(end: bool) {
    let moved = with_state(|st| {
        st.shell
            .active_tab
            .is_some_and(|idx| st.shell.move_tab_to_edge(idx, end))
    })
    .unwrap_or(false);
    if moved {
        crate::refresh_tab_chrome();
        crate::drain_shell();
    }
}

/// Give the active tab `color`, or take its color away.
fn apply_color(color: Option<TabColor>) {
    let changed = with_state(|st| {
        st.shell
            .active_tab
            .is_some_and(|idx| st.shell.set_tab_color(idx, color))
    })
    .unwrap_or(false);
    if changed {
        crate::sync_tab_strip();
    }
}

/// Open the active tab's file in a guest instance
/// ([`codepp_shell::spawn_new_instance`]) and, for a move, close the tab
/// `id` here once the new instance has started.
fn new_instance(id: i32, close_here: bool) {
    let path = with_state(|st| {
        let idx = st.shell.active_tab?;
        let dirty = crate::active_holds_unsaved(st);
        st.shell.new_instance_path(idx, dirty)
    })
    .flatten();
    // The entry is greyed for a tab with no file or unsaved work.
    let Some(path) = path else {
        return;
    };
    if let Err(err) = codepp_shell::spawn_new_instance(&path) {
        tracing::warn!(?err, ?path, "a new instance could not be started");
        crate::message_dialog(
            gtk::MessageType::Warning,
            gtk::ButtonsType::Ok,
            "New Instance",
            &format!(
                "Code++ could not start a new instance.\n\n{}",
                codepp_shell::sanitize_str_for_display(&err.to_string())
            ),
        );
        return;
    }
    if close_here {
        crate::close_tab_by_id(id);
    }
}

/// The menu GTK builds from the shell's tree, which needs a display.
/// Run from `crate::display_tests::gtk_display_scenarios`.
#[cfg(test)]
pub(crate) mod display_tests {
    use super::build_menu;
    use codepp_shell::{Shell, Tab, TabMenuItem};
    use gtk::prelude::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// The text an entry shows, whether a plain label or a swatch's row.
    fn label_of(entry: &gtk::MenuItem) -> Option<String> {
        if let Some(text) = entry.label() {
            return Some(text.to_string());
        }
        let row = entry.child()?.downcast::<gtk::Box>().ok()?;
        row.children()
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Label>().ok())
            .map(|l| l.text().to_string())
    }

    /// Whether `menu` holds `items`, entry for entry: the same text, the
    /// same sensitivity, a submenu where the tree has one, and a square
    /// beside every entry the tree gives a swatch.
    fn check(menu: &gtk::Menu, items: &[TabMenuItem]) {
        let entries = menu.children();
        assert_eq!(
            entries.len(),
            items.len(),
            "the menu has another number of entries"
        );
        for (widget, item) in entries.iter().zip(items) {
            match item {
                TabMenuItem::Separator => {
                    assert!(
                        widget.is::<gtk::SeparatorMenuItem>(),
                        "a separator is missing"
                    );
                }
                TabMenuItem::Command {
                    label,
                    enabled,
                    swatch,
                    ..
                } => {
                    let entry = widget.downcast_ref::<gtk::MenuItem>().expect("an entry");
                    assert_eq!(label_of(entry).as_deref(), Some(*label));
                    assert_eq!(entry.is_sensitive(), *enabled, "`{label}`'s sensitivity");
                    assert!(entry.submenu().is_none(), "`{label}` has a submenu");
                    let square = entry
                        .child()
                        .and_then(|c| c.downcast::<gtk::Box>().ok())
                        .is_some_and(|row| {
                            row.children()
                                .iter()
                                .any(gtk::Widget::is::<gtk::DrawingArea>)
                        });
                    assert_eq!(square, swatch.is_some(), "`{label}`'s square");
                }
                TabMenuItem::Submenu {
                    label,
                    enabled,
                    items,
                } => {
                    let entry = widget.downcast_ref::<gtk::MenuItem>().expect("an entry");
                    assert_eq!(label_of(entry).as_deref(), Some(*label));
                    assert_eq!(entry.is_sensitive(), *enabled, "`{label}`'s sensitivity");
                    let sub = entry
                        .submenu()
                        .and_then(|m| m.downcast::<gtk::Menu>().ok())
                        .unwrap_or_else(|| panic!("`{label}` has no submenu"));
                    check(&sub, items);
                }
            }
        }
    }

    /// The menu for a file on disk and for an untitled buffer is the
    /// shell's tree, entry for entry, with the colors' squares.
    pub(crate) fn the_menu_is_the_shells_tree() {
        gtk::init().expect("display");
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let mut shell = Shell::new(wake).expect("shell");
        shell.tabs.clear();
        shell.tabs.push(Tab {
            id: 1,
            path: Some(PathBuf::from("/tmp/codepp-tab-menu-test.txt")),
            ..Tab::default()
        });
        shell.tabs.push(Tab {
            id: 2,
            ..Tab::default()
        });
        for (idx, id) in [(0, 1), (1, 2)] {
            for dirty in [false, true] {
                let items = shell.tab_context_menu(idx, dirty);
                assert!(!items.is_empty());
                check(&build_menu(&items, id), &items);
            }
        }
        // The two trees differ (Save, Reload, Open into), so matching
        // each one means something.
        assert_ne!(
            shell.tab_context_menu(0, false),
            shell.tab_context_menu(1, false)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::rgb_f64;

    #[test]
    fn rgb_components_are_in_order() {
        let (r, g, b) = rgb_f64(0x00FF_8000);
        assert!((r - 1.0).abs() < 1e-9);
        assert!((g - 128.0 / 255.0).abs() < 1e-9);
        assert!(b.abs() < 1e-9);
    }
}

/// The orders this module rests on, which no runtime test without a
/// display and a plugin can see. Matched on code, not prose
/// ([`crate::source_scan`]).
#[cfg(test)]
mod guards {
    use crate::source_scan::{
        block_after, code_only, depth_one_position, fn_body, strip_test_modules,
    };

    fn source(text: &str) -> String {
        strip_test_modules(&code_only(text))
    }

    /// The press handler only records the press and stops it: the switch,
    /// the delivery and the popup run from the idle `request` queues, not
    /// while GTK is still in the event of a label the switch drops.
    #[test]
    fn the_right_click_only_queues_the_menu() {
        let tabs = source(include_str!("tabs.rs"));
        let arm = block_after(&tabs, "if ev.button() == RIGHT_BUTTON {");
        let flat: String = arm.split_whitespace().collect();
        assert!(
            flat.contains("crate::tab_menu::request(id,"),
            "the right click no longer queues the menu"
        );
        assert!(
            flat.ends_with("returnglib::Propagation::Stop;}"),
            "the right click is no longer stopped at the label"
        );
        for banned in ["show(", "select_tab_by_id", "drain_shell", "popup"] {
            assert!(
                !flat.contains(banned),
                "the press handler calls `{banned}` itself"
            );
        }
        let this = source(include_str!("tab_menu.rs"));
        let request: String = fn_body(&this, "request").split_whitespace().collect();
        assert!(
            request.contains("idle_add_local_full(glib::Priority::HIGH,"),
            "the menu is no longer shown from a high-priority idle"
        );
    }

    /// The menu is for the tab that was clicked: switched to and announced
    /// first, then looked at again, and only then built and shown.
    #[test]
    fn the_menu_is_built_for_the_tab_in_front() {
        let this = source(include_str!("tab_menu.rs"));
        let body = fn_body(&this, "show");
        let at = |needle: &str| {
            depth_one_position(&body, needle)
                .unwrap_or_else(|| panic!("`show` no longer has `{needle}` as a statement"))
        };
        let switch = body
            .find("crate::select_tab_by_id(id);")
            .expect("`show` no longer switches to the tab");
        let drain = body
            .find("crate::drain_shell();")
            .expect("`show` no longer delivers the switch");
        let check = at("if !crate::active_tab_is(id) {\n        return;");
        let built = at("let popup = build_menu(");
        assert!(switch < drain && drain < check && check < built);
    }

    /// No switch, delivery or popup is queued, or run, once a quit has
    /// been asked for or while a modal prompt holds the drain's freeze;
    /// and the popup is placed against the main window, which outlives the
    /// switch, never by the press's own window, which it destroys.
    #[test]
    fn the_menu_waits_for_a_quiet_window_and_places_itself() {
        let this = source(include_str!("tab_menu.rs"));
        for name in ["request", "show"] {
            let body = fn_body(&this, name);
            let refuse = depth_one_position(
                &body,
                "if crate::quit_requested() || crate::DrainFreeze::active() {",
            )
            .unwrap_or_else(|| panic!("`{name}` no longer refuses during a quit or a modal"));
            let first = ["PENDING", "select_tab_by_id"]
                .iter()
                .filter_map(|n| body.find(n))
                .min()
                .unwrap_or(body.len());
            assert!(refuse < first, "`{name}` acts before it checks");
        }
        let show: String = fn_body(&this, "show").split_whitespace().collect();
        assert!(
            show.contains("popup.popup_at_rect(&gdk_window,") && !show.contains("popup_at_pointer"),
            "the menu is no longer placed against the main window"
        );
    }

    /// Every entry checks that its tab is still in front before it acts.
    #[test]
    fn an_entry_acts_only_on_the_tab_it_was_built_for() {
        let this = source(include_str!("tab_menu.rs"));
        let body = fn_body(&this, "run");
        let check = depth_one_position(&body, "if !crate::active_tab_is(id) {")
            .expect("`run` no longer checks the tab is in front");
        let dispatch =
            depth_one_position(&body, "match command {").expect("`run` no longer dispatches");
        assert!(check < dispatch);
    }
}
