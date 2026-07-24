//! The menu bar and its handlers.
//!
//! Wired: File (New, Open, Save, Save As, Save All, Reload, Close, Close
//! All, Exit), Edit (Undo/Redo, Cut/Copy/Paste/Delete, Select All),
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

use std::path::PathBuf;

use codepp_shell::{OpenFileOutcome, UiPlatform};
use gtk::gdk::keys::constants as key;
use gtk::glib;
use gtk::prelude::*;

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

/// Build the menu bar. Handlers are attached separately by [`connect`],
/// because they need the window state installed first.
pub fn build() -> gtk::MenuBar {
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

    // Open Folder as Workspace — pops the folder picker and roots the
    // side tree there. Enabled always, matching Win32.
    let ws = gtk::MenuItem::with_mnemonic("Open Folder as _Workspace…");
    ws.connect_activate(|_| crate::workspace::open_folder_flow());
    menu.append(&ws);
    menu.append(&gtk::SeparatorMenuItem::new());

    // Rename — a real path move (Save As) for a saved file, a display-name
    // change for an untitled buffer. Matches Win32's File → Rename.
    let rename = gtk::MenuItem::with_mnemonic("Rena_me…");
    rename.connect_activate(|_| on_rename());
    menu.append(&rename);

    // Restore Recent Closed File — a persistent item (unlike the rebuilt
    // Recent Files submenu below) so its Ctrl+Shift+T accelerator stays
    // registered in the accel group. A top-level File item here, matching
    // Notepad++. Mnemonic on the `t` (not `R`, which `_Reload` already
    // claims) so it also echoes the Ctrl+Shift+T shortcut. `key::T`
    // uppercase matches this file's convention for Shift combos
    // (`key::S`/`key::W`); GTK normalises either case for a Shift accel.
    let restore = gtk::MenuItem::with_mnemonic("Res_tore Recent Closed File");
    restore.add_accelerator(
        "activate",
        accel,
        *key::T,
        ctrl_shift,
        gtk::AccelFlags::VISIBLE,
    );
    restore.connect_activate(|_| restore_recent_closed());
    menu.append(&restore);

    // The recent-files region — the numbered file list plus the Open All /
    // Empty actions — is rebuilt on every File-menu open: its contents are
    // dynamic, and its shape follows the Preferences "In Submenu" setting
    // (inline flat by default, or nested in a "Recent Files" submenu). It
    // is inserted just above this anchor separator; see
    // `rebuild_recent_region`. (Restore Recent Closed File stays a
    // persistent item above, so its global Ctrl+Shift+T accelerator keeps
    // its binding — the one deliberate divergence from Win32, which nests
    // Restore inside the region.)
    let anchor = gtk::SeparatorMenuItem::new();
    menu.append(&anchor);
    RECENT_ANCHOR.with(|a| *a.borrow_mut() = Some(anchor));
    menu.connect_show(rebuild_recent_region);

    let exit = gtk::MenuItem::with_mnemonic("E_xit");
    exit.connect_activate(|_| {
        save_session_now();
        gtk::main_quit();
    });
    menu.append(&exit);
    menu.show_all();
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
/// feature is inactive the region is empty; otherwise the numbered file
/// list (formatted per the display mode) is followed — after an inner
/// separator when non-empty — by the Open All / Empty actions. With "In
/// Submenu" on, all of that nests inside a single "Recent Files" popup;
/// off (the default) it is inlined flat, ready to insert on the File menu.
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
    let open_all = gtk::MenuItem::with_mnemonic("_Open All Recent Files");
    open_all.set_sensitive(has);
    open_all.connect_activate(|_| open_all_recent());
    let empty = gtk::MenuItem::with_mnemonic("_Empty Recent Files List");
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
        submenu.append(&open_all);
        submenu.append(&empty);
        let parent = gtk::MenuItem::with_mnemonic("Recent _Files");
        parent.set_submenu(Some(&submenu));
        vec![parent.upcast()]
    } else {
        let mut out: Vec<gtk::Widget> = file_items.into_iter().map(Cast::upcast).collect();
        if has {
            out.push(gtk::SeparatorMenuItem::new().upcast());
        }
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
}

impl ViewIndicators {
    const fn new() -> Self {
        Self {
            menu_word_wrap: None,
            menu_show_whitespace: None,
            menu_show_eol: None,
            tb_word_wrap: None,
            tb_show_all_chars: None,
        }
    }
}

/// Register the toolbar's two functional toggle buttons so
/// [`refresh_view_indicators`] can keep them in step with the menu checks.
/// Called by [`crate::toolbar::build_toolbar`].
pub(crate) fn register_toolbar_view_toggles(
    word_wrap: gtk::ToggleToolButton,
    show_all_chars: gtk::ToggleToolButton,
) {
    VIEW_INDICATORS.with(|reg| {
        let mut reg = reg.borrow_mut();
        reg.tb_word_wrap = Some(word_wrap);
        reg.tb_show_all_chars = Some(show_all_chars);
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
        SCI_GETVIEWEOL, SCI_GETVIEWWS, SCI_GETWRAPMODE, SCWS_INVISIBLE, SC_WRAP_NONE,
    };
    let Some((wrap, ws, eol)) = with_state(|st| {
        let e = &st.editor;
        (
            e.send(SCI_GETWRAPMODE, 0, 0) != SC_WRAP_NONE as isize,
            e.send(SCI_GETVIEWWS, 0, 0) != SCWS_INVISIBLE as isize,
            e.send(SCI_GETVIEWEOL, 0, 0) != 0,
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
