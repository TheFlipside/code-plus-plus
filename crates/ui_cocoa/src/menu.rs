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
use objc2::runtime::Sel;
use objc2::{define_class, msg_send, sel, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSControl, NSMenu, NSMenuItem};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSString, NSTimer};

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

    impl Actions {
        #[unsafe(method(codeppNewFile:))]
        fn new_file(&self, _sender: Option<&NSObject>) {
            crate::action_new_file();
        }

        #[unsafe(method(codeppOpenFile:))]
        fn open_file(&self, _sender: Option<&NSObject>) {
            crate::action_open_file();
        }

        #[unsafe(method(codeppSaveFile:))]
        fn save_file(&self, _sender: Option<&NSObject>) {
            crate::action_save_file();
        }

        #[unsafe(method(codeppSaveFileAs:))]
        fn save_file_as(&self, _sender: Option<&NSObject>) {
            crate::action_save_file_as();
        }

        #[unsafe(method(codeppSaveAll:))]
        fn save_all(&self, _sender: Option<&NSObject>) {
            crate::action_save_all();
        }

        #[unsafe(method(codeppReload:))]
        fn reload(&self, _sender: Option<&NSObject>) {
            crate::action_reload();
        }

        /// Close the tab whose `Tab.id` is in the sender's `tag`.
        #[unsafe(method(codeppCloseTab:))]
        fn close_tab(&self, sender: Option<&NSControl>) {
            if let Some(sender) = sender {
                crate::close_tab_by_id(sender.tag() as i32);
            }
        }

        #[unsafe(method(codeppCloseCurrentTab:))]
        fn close_current_tab(&self, _sender: Option<&NSObject>) {
            crate::action_close_tab();
        }

        /// The 7-second session auto-save. Win32 uses `SetTimer` +
        /// `WM_TIMER` and GTK `g_timeout_add_seconds`; `NSTimer` is the
        /// direct Cocoa analogue and fires on the main run loop, so it
        /// can touch the editor safely.
        #[unsafe(method(codeppAutosaveTick:))]
        fn autosave_tick(&self, _timer: Option<&NSObject>) {
            crate::save_session_now();
        }
    }
);

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
) {
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
}
