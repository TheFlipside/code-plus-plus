//! The menu bar, and the Objective-C target that receives its actions.
//!
//! # Why an action target class exists
//!
//! AppKit dispatches a menu item by sending a selector, either to an
//! explicit target or — for a nil target — down the responder chain.
//! The Edit menu uses the nil-target form, because the selectors it
//! sends (`undo:`, `selectAll:`, …) are ones Scintilla's content view
//! already implements, so they land on the editor with no code of ours
//! in between. That is why m1 got working undo/redo/clipboard for free.
//!
//! Code++'s own commands have no such pre-existing implementor, so they
//! need a receiver: [`Actions`], a small `NSObject` subclass whose
//! methods hop straight into the Rust handlers. It also owns the
//! auto-save timer's callback, since `NSTimer` likewise wants a
//! target/selector pair.
//!
//! # Menu layout
//!
//! Per the platform-conventions decision recorded in the Phase 5 plan:
//! macOS-native where the platform mandates it, Notepad++ everywhere
//! else. So About / Settings / Quit live in the application menu rather
//! than under `?` / `Settings` / `File`, and the accelerators are ⌘-based
//! equivalents of the Ctrl bindings in `ui_win32`'s accelerator table.
//! The right-justified `＋ ▼ X` group has no `NSMenu` expression and is
//! dropped.
//!
//! The full Notepad++ menu tree (Search, View, Encoding, Language,
//! Tools, Macro, Run, Plugins, Window) arrives in m3 with the tab strip
//! and the dialogs it drives.

use objc2::rc::Retained;
use objc2::runtime::{ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSControl, NSMenu, NSMenuDelegate, NSMenuItem, NSWorkspace};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSString, NSTimer, NSURL};

use crate::AUTOSAVE_INTERVAL_SECS;

define_class!(
    // SAFETY: plain `NSObject` subclass with no ivars; every method
    // below is invoked by AppKit on the main thread, which is also
    // where the `thread_local` state it reaches lives.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppMenuActions"]
    pub struct Actions;

    unsafe impl NSObjectProtocol for Actions {}

    unsafe impl NSMenuDelegate for Actions {
        /// Regenerate a menu whose contents depend on live model state,
        /// just before it is shown.
        ///
        /// Only the Window menu needs it today: its buffer list changes
        /// with every open, close and reorder, and rebuilding on demand
        /// beats pushing an update from every one of those paths — the
        /// same reasoning that makes `TabStrip::sync` total rather than
        /// incremental. The menu is identified by its tag so one delegate
        /// can serve any future menu with the same need.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            crate::at_callback_boundary("menu:needsUpdate", (), || {
                if is_window_menu(menu) {
                    rebuild_window_menu(menu, self);
                }
            });
        }
    }

    impl Actions {
        #[unsafe(method(codeppNewFile:))]
        fn new_file(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:new_file", (), crate::action_new_file);
        }

        #[unsafe(method(codeppOpenFile:))]
        fn open_file(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:open_file", (), crate::action_open_file);
        }

        #[unsafe(method(codeppSaveFile:))]
        fn save_file(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:save_file", (), crate::action_save_file);
        }

        #[unsafe(method(codeppSaveFileAs:))]
        fn save_file_as(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:save_file_as", (), crate::action_save_file_as);
        }

        #[unsafe(method(codeppSaveAll:))]
        fn save_all(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:save_all", (), crate::action_save_all);
        }

        #[unsafe(method(codeppReload:))]
        fn reload(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:reload", (), crate::action_reload);
        }

        /// Close the tab whose `Tab.id` is in the sender's `tag`.
        #[unsafe(method(codeppCloseTab:))]
        fn close_tab(&self, sender: Option<&NSControl>) {
            crate::at_callback_boundary("menu:closeTab", (), || {
                if let Some(sender) = sender {
                    crate::close_tab_by_id(sender.tag() as i32);
                }
            });
        }

        #[unsafe(method(codeppCloseCurrentTab:))]
        fn close_current_tab(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:close_current_tab", (), || {
                crate::action_close_tab();
            });
        }

        /// The 7-second session auto-save. Win32 uses `SetTimer` +
        /// `WM_TIMER` and GTK `g_timeout_add_seconds`; `NSTimer` is the
        /// direct Cocoa analogue and fires on the main run loop, so it
        /// can touch the editor safely.
        #[unsafe(method(codeppAutosaveTick:))]
        fn autosave_tick(&self, _timer: Option<&NSObject>) {
            crate::at_callback_boundary("menu:autosaveTick", (), crate::save_session_now);
        }

        // ---- m3d: the rest of the Notepad++ menu tree ----------------

        #[unsafe(method(codeppCloseAll:))]
        fn close_all(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:closeAll", (), crate::action_close_all);
        }

        #[unsafe(method(codeppZoomIn:))]
        fn zoom_in(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:zoomIn", (), || {
                crate::editor_cmd(codepp_scintilla_sys::SCI_ZOOMIN);
            });
        }

        #[unsafe(method(codeppZoomOut:))]
        fn zoom_out(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:zoomOut", (), || {
                crate::editor_cmd(codepp_scintilla_sys::SCI_ZOOMOUT);
            });
        }

        #[unsafe(method(codeppZoomReset:))]
        fn zoom_reset(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:zoomReset", (), crate::reset_zoom);
        }

        /// One selector for all four View toggles, keyed by the item's
        /// `tag`. Four near-identical methods would be four places to
        /// forget to update; the tag is already how every other item here
        /// carries its parameter.
        #[unsafe(method(codeppToggleView:))]
        fn toggle_view(&self, sender: Option<&NSMenuItem>) {
            crate::at_callback_boundary("menu:toggleView", (), || {
                if let Some(sender) = sender {
                    crate::toggle_view_setting_by_tag(sender.tag());
                }
            });
        }

        #[unsafe(method(codeppSetEncoding:))]
        fn set_encoding(&self, sender: Option<&NSMenuItem>) {
            crate::at_callback_boundary("menu:setEncoding", (), || {
                if let Some(sender) = sender {
                    crate::apply_encoding_by_tag(sender.tag());
                }
            });
        }

        #[unsafe(method(codeppSetLanguage:))]
        fn set_language(&self, sender: Option<&NSMenuItem>) {
            crate::at_callback_boundary("menu:setLanguage", (), || {
                if let Some(sender) = sender {
                    crate::apply_language(sender.tag() as i32);
                }
            });
        }

        /// Window-menu buffer switch. Carries `Tab.id`, like every other
        /// tab-addressing control in this backend.
        #[unsafe(method(codeppSelectTabMenu:))]
        fn select_tab_menu(&self, sender: Option<&NSMenuItem>) {
            crate::at_callback_boundary("menu:selectTab", (), || {
                if let Some(sender) = sender {
                    crate::select_tab_by_id(sender.tag() as i32);
                }
            });
        }

        #[unsafe(method(codeppOpenHelpUrl:))]
        fn open_help_url(&self, sender: Option<&NSMenuItem>) {
            crate::at_callback_boundary("menu:openHelpUrl", (), || {
                if let Some(sender) = sender {
                    open_help_url(sender.tag());
                }
            });
        }

        #[unsafe(method(codeppRestoreWindowSize:))]
        fn restore_window_size(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary(
                "menu:restoreWindowSize",
                (),
                crate::restore_default_window_size,
            );
        }

        /// AppKit's per-item enable/disable hook, which is also where a
        /// check or radio mark belongs.
        ///
        /// **This runs only because automatic menu enabling is left on.**
        /// AppKit drives the whole validation protocol from that flag, so
        /// a `setAutoenablesItems(false)` anywhere in this file would
        /// silently stop every check and radio mark below from ever being
        /// painted — the menus would still open and every command would
        /// still work, which is what makes it worth stating. It is also
        /// what greys the not-yet-implemented placeholders: they carry no
        /// action, so AppKit finds no implementor. See [`add_disabled`].
        ///
        /// **Why state is set here rather than when the item is built.**
        /// A language, encoding or View toggle can change from somewhere
        /// other than its own menu item — a tab switch, a plugin, the
        /// toolbar — so a mark written once at build time drifts. AppKit
        /// calls this for every item just before its menu is displayed,
        /// which is exactly the refresh point `ui_gtk` gets from a
        /// `connect_show` handler per menu. Doing it here means one place
        /// instead of three, and no `REFRESHING_MARKS`-style re-entrancy
        /// flag: setting `state` does not fire the item's action, whereas
        /// GTK's `set_active` does.
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            crate::at_callback_boundary("menu:validate", true, || validate(item))
        }
    }
);

/// Compute (and apply) the display state of one menu item.
///
/// Returns whether the item is enabled. Items with no action, or with an
/// action this does not recognise, are left alone and enabled — that
/// covers the Edit menu's responder-chain items, whose enabling AppKit
/// resolves against the Scintilla view rather than against us.
fn validate(item: &NSMenuItem) -> bool {
    let Some(action) = item.action() else {
        return true;
    };
    let tag = item.tag();
    if action == sel!(codeppSetLanguage:) {
        let active = crate::active_lang_id();
        item.setState(isize::from(active == Some(tag as i32)));
    } else if action == sel!(codeppSetEncoding:) {
        item.setState(isize::from(crate::active_encoding_tag() == Some(tag)));
    } else if action == sel!(codeppToggleView:) {
        item.setState(isize::from(crate::view_setting_by_tag(tag)));
    } else if action == sel!(codeppSelectTabMenu:) {
        item.setState(isize::from(crate::active_tab_id() == Some(tag as i32)));
    }
    true
}

/// Open one of the fixed help URLs in the default browser.
///
/// The URL is chosen by tag from a compile-time table — no user- or
/// plugin-supplied string ever reaches `NSWorkspace`, which is the same
/// property `ui_gtk::open_uri`'s call sites hold.
fn open_help_url(tag: isize) {
    let Some(url) = HELP_URLS.get(usize::try_from(tag).unwrap_or(usize::MAX)) else {
        return;
    };
    let Some(url) = NSURL::URLWithString(&NSString::from_str(url.1)) else {
        return;
    };
    NSWorkspace::sharedWorkspace().openURL(&url);
}

impl Actions {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `init` on a freshly allocated instance of our own
        // class, which adds no ivars needing other initialisation.
        unsafe { msg_send![this, init] }
    }

    /// Start the repeating session auto-save.
    ///
    /// The returned timer is retained by the run loop, so the caller
    /// does not have to keep it alive; `self` must outlive it, which the
    /// caller guarantees by holding [`Actions`] in the window state.
    pub fn start_autosave(&self, mtm: MainThreadMarker) -> Retained<NSTimer> {
        let _ = mtm;
        // SAFETY: `self` implements `codeppAutosaveTick:` — declared by
        // the `define_class!` block above, so the selector cannot be
        // stale. `NSTimer` strongly retains its target, and the caller
        // additionally holds `Actions` for the process lifetime, so the
        // target outlives every firing.
        unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                AUTOSAVE_INTERVAL_SECS,
                self,
                sel!(codeppAutosaveTick:),
                None,
                true,
            )
        }
    }
}

/// Build and install the whole menu bar.
///
/// `actions` receives every Code++ command; the Edit menu deliberately
/// uses a nil target so its items reach Scintilla through the responder
/// chain (see the module docs).
pub fn install(app: &NSApplication, actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenu> {
    let main_menu = NSMenu::new(mtm);
    main_menu.addItem(&build_app_menu(mtm));
    main_menu.addItem(&build_file_menu(actions, mtm));
    main_menu.addItem(&build_edit_menu(mtm));
    main_menu.addItem(&build_search_menu(mtm));
    main_menu.addItem(&build_view_menu(actions, mtm));
    main_menu.addItem(&build_encoding_menu(actions, mtm));
    main_menu.addItem(&build_language_menu(actions, mtm));
    main_menu.addItem(&build_window_menu(actions, mtm));
    main_menu.addItem(&build_help_menu(actions, mtm));
    app.setMainMenu(Some(&main_menu));
    main_menu
}

/// The application menu.
///
/// macOS mandates this menu and gives it the process name. The first
/// item of the main menu is *always* treated as the application menu
/// regardless of its title, so the title here is never displayed.
fn build_app_menu(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    add(
        &app_menu,
        mtm,
        "About Code++",
        sel!(orderFrontStandardAboutPanel:),
        "",
        None,
    );
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add(&app_menu, mtm, "Hide Code++", sel!(hide:), "h", None);
    add(
        &app_menu,
        mtm,
        "Hide Others",
        sel!(hideOtherApplications:),
        "",
        None,
    );
    add(
        &app_menu,
        mtm,
        "Show All",
        sel!(unhideAllApplications:),
        "",
        None,
    );
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add(&app_menu, mtm, "Quit Code++", sel!(terminate:), "q", None);
    app_item.setSubmenu(Some(&app_menu));
    app_item
}

/// The File menu — the subset m2 implements.
///
/// Notepad++'s ordering and wording, with ⌘ equivalents of
/// `ui_win32`'s Ctrl accelerators. Close/Close All need the tab strip
/// and arrive in m3; Exit lives in the application menu on this
/// platform.
fn build_file_menu(actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let file_item = NSMenuItem::new(mtm);
    let file_menu = NSMenu::new(mtm);
    file_menu.setTitle(&NSString::from_str("File"));
    add(
        &file_menu,
        mtm,
        "New",
        sel!(codeppNewFile:),
        "n",
        Some(actions),
    );
    add(
        &file_menu,
        mtm,
        "Open…",
        sel!(codeppOpenFile:),
        "o",
        Some(actions),
    );
    add(
        &file_menu,
        mtm,
        "Reload from Disk",
        sel!(codeppReload:),
        "r",
        Some(actions),
    );
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add(
        &file_menu,
        mtm,
        "Save",
        sel!(codeppSaveFile:),
        "s",
        Some(actions),
    );
    // ⇧⌘S. An uppercase key equivalent implies Shift by AppKit
    // convention, which is why no explicit modifier mask is set.
    add(
        &file_menu,
        mtm,
        "Save As…",
        sel!(codeppSaveFileAs:),
        "S",
        Some(actions),
    );
    add(
        &file_menu,
        mtm,
        "Save All",
        sel!(codeppSaveAll:),
        "",
        Some(actions),
    );
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add(
        &file_menu,
        mtm,
        "Close",
        sel!(codeppCloseCurrentTab:),
        "w",
        Some(actions),
    );
    add(
        &file_menu,
        mtm,
        "Close All",
        sel!(codeppCloseAll:),
        "W",
        Some(actions),
    );
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Printing needs an `NSPrintOperation` driving Scintilla's
    // `SCI_FORMATRANGEFULL`, which is its own milestone — greyed here for
    // the same reason DESIGN.md §7.2 keeps it greyed on Win32.
    add_disabled(&file_menu, mtm, "Print…");
    file_item.setSubmenu(Some(&file_menu));
    file_item
}

/// The Edit menu.
///
/// Every item targets a **standard AppKit action selector with a nil
/// target**, so AppKit sends it down the responder chain and it lands on
/// whichever view is first responder. Scintilla's content view
/// implements all of them (`cocoa/ScintillaView.mm:913` `selectAll:`,
/// `:928` `copy:`, `:943` `undo:`, and the rest alongside) plus
/// `validateUserInterfaceItem:` (`:966`), so the items also grey
/// themselves out correctly with no work on our side.
fn build_edit_menu(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let edit_item = NSMenuItem::new(mtm);
    let edit_menu = NSMenu::new(mtm);
    edit_menu.setTitle(&NSString::from_str("Edit"));
    add(&edit_menu, mtm, "Undo", sel!(undo:), "z", None);
    add(&edit_menu, mtm, "Redo", sel!(redo:), "Z", None);
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add(&edit_menu, mtm, "Cut", sel!(cut:), "x", None);
    add(&edit_menu, mtm, "Copy", sel!(copy:), "c", None);
    add(&edit_menu, mtm, "Paste", sel!(paste:), "v", None);
    add(&edit_menu, mtm, "Delete", sel!(delete:), "", None);
    add(&edit_menu, mtm, "Select All", sel!(selectAll:), "a", None);
    edit_item.setSubmenu(Some(&edit_menu));
    edit_item
}

// --- m3d: the rest of the tree ----------------------------------------

/// The `?` menu's external links, indexed by the menu item's tag.
///
/// Compile-time table rather than a per-item string, so
/// [`open_help_url`] can never be handed a URL that came from a file, a
/// plugin or a user. The URLs themselves must agree with `ui_win32`'s
/// and `ui_gtk`'s `HELP_*_URL` constants.
const HELP_URLS: &[(&str, &str)] = &[
    ("Code++ Home", "https://code-plus-plus.org/"),
    (
        "Code++ Project Page",
        "https://github.com/TheFlipside/code-plus-plus",
    ),
    (
        "Code++ Community (Forum)",
        "https://community.code-plus-plus.org/",
    ),
];

/// The Encoding menu's rows, indexed by the item's tag.
///
/// `None` is ANSI, which Code++ does not offer as a *save* encoding —
/// listed and disabled so the menu reads the same as Notepad++'s rather
/// than silently omitting a row users look for. Same treatment `ui_gtk`
/// gives it.
fn encoding_rows() -> [(&'static str, Option<codepp_core::Encoding>); 5] {
    [
        ("ANSI", None),
        ("UTF-8 (no BOM)", Some(codepp_core::Encoding::Utf8)),
        ("UTF-8 with BOM", Some(codepp_core::Encoding::Utf8Bom)),
        ("UTF-16 LE BOM", Some(codepp_core::Encoding::Utf16LeBom)),
        ("UTF-16 BE BOM", Some(codepp_core::Encoding::Utf16BeBom)),
    ]
}

/// The Search menu.
///
/// Every item is present but disabled: each one needs a dialog, and the
/// dialogs are m4. Listing them greyed rather than omitting the menu is
/// deliberate and matches how both other backends handle a
/// not-yet-implemented command (Win32's `MF_GRAYED` File-menu
/// placeholders, `ui_gtk`'s greyed "Online User Manual"): the tree a user
/// navigates stays the one Notepad++ has, and the gap is visible instead
/// of looking like a missing feature.
fn build_search_menu(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let (item, menu) = submenu("Search", mtm);
    // No key equivalents yet: AppKit would swallow the chord for a
    // command that cannot run, so ⌘F would do nothing rather than
    // falling through. They arrive with the dialogs in m4.
    for label in [
        "Find…",
        "Replace…",
        "Find in Files…",
        "Find Next",
        "Find Previous",
        "Go to…",
    ] {
        add_disabled(&menu, mtm, label);
    }
    item
}

/// The View menu: zoom, the display toggles, and a window-size reset.
fn build_view_menu(actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let (item, menu) = submenu("View", mtm);
    add(
        &menu,
        mtm,
        "Zoom In",
        sel!(codeppZoomIn:),
        "+",
        Some(actions),
    );
    add(
        &menu,
        mtm,
        "Zoom Out",
        sel!(codeppZoomOut:),
        "-",
        Some(actions),
    );
    add(
        &menu,
        mtm,
        "Restore Default Zoom",
        sel!(codeppZoomReset:),
        "0",
        Some(actions),
    );
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Tags index `crate::VIEW_TOGGLES`; the mark is written by
    // `validateMenuItem:` from the live setting rather than tracked here.
    for (tag, label) in crate::VIEW_TOGGLES.iter().enumerate() {
        let entry = add(
            &menu,
            mtm,
            label,
            sel!(codeppToggleView:),
            "",
            Some(actions),
        );
        entry.setTag(isize::try_from(tag).unwrap_or(0));
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add(
        &menu,
        mtm,
        "Restore Default Window Size",
        sel!(codeppRestoreWindowSize:),
        "",
        Some(actions),
    );
    item
}

/// The Encoding menu.
fn build_encoding_menu(actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let (item, menu) = submenu("Encoding", mtm);
    for (tag, (label, encoding)) in encoding_rows().into_iter().enumerate() {
        if encoding.is_none() {
            add_disabled(&menu, mtm, label);
            continue;
        }
        let entry = add(
            &menu,
            mtm,
            label,
            sel!(codeppSetEncoding:),
            "",
            Some(actions),
        );
        entry.setTag(isize::try_from(tag).unwrap_or(0));
    }
    item
}

/// The Language menu: every entry in `core`'s language table.
///
/// Laid out exactly as `ui_gtk` lays it out, because the table is the
/// same and users move between the two: Normal Text pinned at the top,
/// then a separator, then the rest alphabetically with same-initial runs
/// collapsed into a lettered submenu. ~88 flat items would be a menu
/// taller than the screen.
fn build_language_menu(actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let (item, menu) = submenu("Language", mtm);
    let table = codepp_core::lang::LANG_TABLE;

    if let Some(first) = table.first() {
        add_language(
            &menu,
            first.menu_label,
            first.lang.as_npp_id(),
            actions,
            mtm,
        );
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    let rest = &table[1..];
    let mut i = 0;
    while i < rest.len() {
        let letter = first_letter(rest[i].menu_label);
        let mut j = i + 1;
        while j < rest.len() && first_letter(rest[j].menu_label) == letter {
            j += 1;
        }
        if j - i == 1 {
            add_language(
                &menu,
                rest[i].menu_label,
                rest[i].lang.as_npp_id(),
                actions,
                mtm,
            );
        } else {
            let (parent, sub) = submenu(&letter.to_string(), mtm);
            for entry in &rest[i..j] {
                add_language(&sub, entry.menu_label, entry.lang.as_npp_id(), actions, mtm);
            }
            menu.addItem(&parent);
        }
        i = j;
    }
    item
}

/// Uppercased first character of a language label, for letter grouping.
/// Non-alphabetic or empty labels floor at a space, keeping them
/// together — same rule `ui_gtk` uses so the two menus group identically.
fn first_letter(label: &str) -> char {
    label.chars().next().map_or(' ', |c| c.to_ascii_uppercase())
}

/// One language row, carrying its Notepad++ language id in the tag.
fn add_language(
    menu: &NSMenu,
    label: &str,
    lang_id: i32,
    actions: &Actions,
    mtm: MainThreadMarker,
) {
    let entry = add(
        menu,
        mtm,
        label,
        sel!(codeppSetLanguage:),
        "",
        Some(actions),
    );
    entry.setTag(lang_id as isize);
}

/// The Window menu: the standard macOS items plus one row per open
/// buffer.
///
/// The buffer rows are rebuilt every time the menu opens rather than
/// tracked as tabs change — see [`Actions`]'s `menuNeedsUpdate:`. On this
/// platform the Window menu is where a user expects to find the list of
/// what is open, which is also where Notepad++ puts it.
fn build_window_menu(actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let (item, menu) = submenu(WINDOW_MENU_TITLE, mtm);
    add(&menu, mtm, "Minimize", sel!(performMiniaturize:), "m", None);
    add(&menu, mtm, "Zoom", sel!(performZoom:), "", None);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Everything past this separator is regenerated on open.
    // SAFETY: `Actions` implements `NSMenuDelegate`, and the delegate
    // reference AppKit holds is weak — the window state owns `actions`
    // for the process lifetime, so it cannot dangle.
    menu.setDelegate(Some(ProtocolObject::from_ref(actions)));
    item
}

/// The `?` menu.
///
/// About lives in the application menu on macOS (the platform mandates
/// it), so this carries the links and the two placeholders Win32 and GTK
/// also grey out.
fn build_help_menu(actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let (item, menu) = submenu("?", mtm);
    for (tag, (label, _)) in HELP_URLS.iter().enumerate() {
        let entry = add(
            &menu,
            mtm,
            label,
            sel!(codeppOpenHelpUrl:),
            "",
            Some(actions),
        );
        entry.setTag(isize::try_from(tag).unwrap_or(0));
    }
    add_disabled(&menu, mtm, "Code++ Online User Manual");
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_disabled(&menu, mtm, "Update Code++");
    item
}

/// Replace the Window menu's buffer rows with the current tab list.
///
/// Everything up to and including the separator that `build_window_menu`
/// installed is fixed; everything after it is ours to regenerate. The
/// count is recomputed rather than remembered so this stays correct if
/// the fixed part grows.
fn rebuild_window_menu(menu: &NSMenu, actions: &Actions) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(rows) = crate::open_buffer_rows() else {
        // Unreachable state (a re-entrant call). Leave the rows that are
        // already there rather than rebuilding the menu into an empty
        // list — see `open_buffer_rows`.
        return;
    };
    // The fixed prefix is stated, not inferred. An earlier version
    // located it by finding the first separator, which is correct today
    // but fails open: remove or reorder that separator and `fixed` drops
    // to 0, so the rebuild silently deletes Minimize and Zoom as well.
    let fixed = WINDOW_MENU_FIXED_ITEMS;
    debug_assert!(
        menu.numberOfItems() >= fixed
            && menu
                .itemArray()
                .iter()
                .nth(usize::try_from(fixed - 1).unwrap_or(0))
                .is_some_and(|item| item.isSeparatorItem()),
        "the Window menu's fixed prefix changed shape; update WINDOW_MENU_FIXED_ITEMS"
    );
    while menu.numberOfItems() > fixed {
        menu.removeItemAtIndex(menu.numberOfItems() - 1);
    }
    for (id, name) in rows {
        let entry = add(
            menu,
            mtm,
            &name,
            sel!(codeppSelectTabMenu:),
            "",
            Some(actions),
        );
        entry.setTag(id as isize);
    }
}

/// How many items at the head of the Window menu are fixed: Minimize,
/// Zoom, and the separator that closes the group. Everything after them
/// is regenerated per open by [`rebuild_window_menu`], which asserts this
/// still describes what [`build_window_menu`] built.
const WINDOW_MENU_FIXED_ITEMS: isize = 3;

/// Title identifying the Window menu to the shared menu delegate.
///
/// `NSMenu` has no `tag`, unlike `NSMenuItem` — hence a title match
/// rather than the tag-keying used everywhere else in this file. Safe
/// because the delegate is installed on exactly one menu and the title
/// is set from this same constant.
const WINDOW_MENU_TITLE: &str = "Window";

/// True if `menu` is the one [`build_window_menu`] built.
fn is_window_menu(menu: &NSMenu) -> bool {
    menu.title().to_string() == WINDOW_MENU_TITLE
}

/// Build an empty submenu and the bar item that owns it.
fn submenu(title: &str, mtm: MainThreadMarker) -> (Retained<NSMenuItem>, Retained<NSMenu>) {
    let item = NSMenuItem::new(mtm);
    let menu = NSMenu::new(mtm);
    // **Both** titles, and both are needed. A menu-bar item takes its
    // label from the *submenu*'s title, but a nested submenu item — the
    // Language menu's letter groups — takes it from the *item*'s, and
    // setting only the menu leaves those rendering blank.
    menu.setTitle(&NSString::from_str(title));
    item.setTitle(&NSString::from_str(title));
    item.setSubmenu(Some(&menu));
    (item, menu)
}

/// Append a permanently disabled item — a command that belongs in the
/// tree but is not implemented yet. See [`build_search_menu`].
///
/// Disabled by having **no action**, not by `setEnabled(false)`. Under
/// AppKit's automatic menu enabling — which this file relies on, see
/// [`validate`] — an item with no action has no implementor to find and
/// is greyed for us, while an explicit `setEnabled` would simply be
/// overwritten on the next validation pass.
fn add_disabled(menu: &NSMenu, mtm: MainThreadMarker, title: &str) {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    menu.addItem(&item);
}

/// Append one menu item.
///
/// `target: None` requests responder-chain dispatch — AppKit walks from
/// the first responder outwards looking for something implementing
/// `action`. That is what lets the Edit menu reach the Scintilla view.
/// `Some(actions)` pins the item to our own receiver instead.
///
/// `key` is the key equivalent without the ⌘; an empty string means no
/// shortcut. An uppercase letter implies Shift by AppKit convention.
fn add(
    menu: &NSMenu,
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key: &str,
    target: Option<&Actions>,
) -> Retained<NSMenuItem> {
    // SAFETY: standard `NSMenuItem` designated initialiser. The selector
    // is a compile-time `sel!` literal, and a nil target is explicitly
    // supported — it is what requests responder-chain dispatch.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    if let Some(target) = target {
        // `setTarget:` holds a weak (unowned) reference, so `Actions`
        // must outlive the menu — the window state owns it for the life
        // of the process.
        unsafe { item.setTarget(Some(target)) };
    }
    menu.addItem(&item);
    item
}
