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
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, AnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAboutPanelOptionApplicationIcon, NSAboutPanelOptionApplicationName,
    NSAboutPanelOptionApplicationVersion, NSAboutPanelOptionCredits, NSAboutPanelOptionKey,
    NSAboutPanelOptionVersion, NSApplication, NSButton, NSControl, NSEventModifierFlags, NSImage,
    NSMenu, NSMenuDelegate, NSMenuItem, NSTableView, NSWindowDelegate, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSData, NSDictionary, NSNotification, NSObject,
    NSObjectProtocol, NSSize, NSString, NSTimer, NSURL,
};

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

    unsafe impl NSWindowDelegate for Actions {
        /// Re-lay the tab strip when the window changes size.
        ///
        /// **Nothing else does.** The strip's container is
        /// `ViewWidthSizable` so it tracks the window for free, but the
        /// tab buttons inside carry no autoresizing mask — they are
        /// positioned by `TabStrip::sync`, which only runs on tab-list
        /// events. Without this hook, narrowing a window until the tabs
        /// overflow produced no arrows (silently reproducing the bug they
        /// exist to fix) and widening one left the strip scrolled, with
        /// early tabs stranded off the left edge, until some unrelated
        /// edit or save happened to refresh the chrome.
        ///
        /// On every resize step rather than only at
        /// `windowDidEndLiveResize:`: the arrows should appear as the
        /// window narrows past the overflow point, not a moment after the
        /// user lets go. The strip is a few dozen small controls and is
        /// rebuilt wholesale anyway.
        /// It also re-lays the chrome bands, which matters only once the
        /// Find-in-Files dock is open: the dock is a fixed-height strip,
        /// so autoresizing gives the whole vertical delta to the editor
        /// and a window shrunk far enough would starve it while the dock
        /// kept its old height. `relayout_chrome` re-derives every band
        /// and re-clamps the dock, which is what `ui_win32` does from its
        /// own `WM_SIZE`.
        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            crate::at_callback_boundary("window:didResize", (), || {
                crate::relayout_chrome_bands();
                crate::refresh_tab_chrome();
            });
        }
    }

    unsafe impl NSMenuDelegate for Actions {
        /// Regenerate a menu whose contents depend on live model state,
        /// just before it is shown.
        ///
        /// Two menus need it. The Window menu's buffer list changes with
        /// every open, close and reorder; the Language menu's loaded-UDL
        /// rows cannot be built at construction at all, because
        /// [`build_language_menu`] runs before `Shell::new` and so before
        /// any UDL registry exists. In both cases rebuilding on demand
        /// beats pushing an update from every path that could invalidate
        /// the list — the same reasoning that makes `TabStrip::sync`
        /// total rather than incremental.
        ///
        /// Each menu is identified by its **title** ([`is_window_menu`],
        /// [`is_language_menu`]), so one delegate serves both and any
        /// future menu with the same need.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            crate::at_callback_boundary("menu:needsUpdate", (), || {
                if is_window_menu(menu) {
                    rebuild_window_menu(menu, self);
                } else if is_language_menu(menu) {
                    rebuild_udl_rows(menu, self);
                    // After the rebuild, so a UDL row added just now can
                    // carry the mark on the pass that created it.
                    mark_language_letter_groups(menu);
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

        /// The toolbar's Word Wrap toggle.
        ///
        /// Separate selectors from the View menu's single
        /// `codeppToggleView:` because these are `PushOnPushOff` buttons:
        /// AppKit has already flipped the button's own state by the time
        /// the action fires, so the handler must *apply* that state rather
        /// than invert the model — inverting would fight the button and
        /// land on the wrong value every other click.
        #[unsafe(method(codeppToolbarWordWrap:))]
        fn toolbar_word_wrap(&self, sender: Option<&NSButton>) {
            crate::at_callback_boundary("toolbar:wordWrap", (), || {
                if let Some(sender) = sender {
                    crate::set_view_setting(0, sender.state() != 0);
                }
            });
        }

        /// The toolbar's Show All Characters toggle — whitespace *and*
        /// EOL together, matching Win32's combined button.
        #[unsafe(method(codeppToolbarShowAllChars:))]
        fn toolbar_show_all_chars(&self, sender: Option<&NSButton>) {
            crate::at_callback_boundary("toolbar:showAllChars", (), || {
                if let Some(sender) = sender {
                    crate::set_show_all_chars(sender.state() != 0);
                }
            });
        }

        /// The toolbar's Show Indent Guide toggle.
        #[unsafe(method(codeppToolbarIndentGuide:))]
        fn toolbar_indent_guide(&self, sender: Option<&NSButton>) {
            crate::at_callback_boundary("toolbar:indentGuide", (), || {
                if let Some(sender) = sender {
                    crate::set_view_setting(3, sender.state() != 0);
                }
            });
        }

        // ---- m4c: the Document Map ---------------------------------

        /// The View menu's Document Map entry. The mark is resolved in
        /// `validateMenuItem:` from the live panel state, so this only
        /// has to flip it.
        #[unsafe(method(codeppToggleDocMap:))]
        fn toggle_doc_map(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("view:toggleDocMap", (), crate::docmap::toggle_from_menu);
        }

        /// The toolbar's Document Map toggle. `PushOnPushOff`, so AppKit
        /// has already flipped the button by the time this fires — apply
        /// its state rather than inverting the model, or the two land on
        /// opposite values every other click.
        #[unsafe(method(codeppToolbarDocMap:))]
        fn toolbar_doc_map(&self, sender: Option<&NSButton>) {
            crate::at_callback_boundary("toolbar:docMap", (), || {
                if let Some(sender) = sender {
                    crate::docmap::set_from_toolbar(sender.state() != 0);
                }
            });
        }

        /// The ✕ in the Document Map's own header.
        #[unsafe(method(codeppDocMapClose:))]
        fn doc_map_close(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary(
                "docmap:close",
                (),
                crate::docmap::close_from_header,
            );
        }

        // ---- m4d: the workspace tree -------------------------------

        /// The View menu's "Folder as Workspace" entry. With no folder
        /// open yet this routes to the picker rather than showing an
        /// empty panel — see `workspace::set_visible`.
        #[unsafe(method(codeppToggleWorkspace:))]
        fn toggle_workspace(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("view:toggleWorkspace", (), || {
                crate::workspace::set_visible(!crate::workspace::is_visible());
            });
        }

        /// File → Open Folder as Workspace…
        #[unsafe(method(codeppOpenFolderAsWorkspace:))]
        fn open_folder_as_workspace(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary(
                "file:openFolderAsWorkspace",
                (),
                crate::workspace::open_folder_flow,
            );
        }

        /// The toolbar's toggle. `PushOnPushOff`, so apply the button's
        /// state rather than inverting the model.
        #[unsafe(method(codeppToolbarWorkspace:))]
        fn toolbar_workspace(&self, sender: Option<&NSButton>) {
            crate::at_callback_boundary("toolbar:workspace", (), || {
                if let Some(sender) = sender {
                    crate::workspace::set_visible(sender.state() != 0);
                }
            });
        }

        #[unsafe(method(codeppWorkspaceClose:))]
        fn workspace_close(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("workspace:close", (), || {
                crate::workspace::set_visible(false);
            });
        }

        #[unsafe(method(codeppWorkspaceFoldAll:))]
        fn workspace_fold_all(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("workspace:foldAll", (), crate::workspace::fold_all);
        }

        #[unsafe(method(codeppWorkspaceUnfoldAll:))]
        fn workspace_unfold_all(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("workspace:unfoldAll", (), crate::workspace::unfold_all);
        }

        #[unsafe(method(codeppWorkspaceLocate:))]
        fn workspace_locate(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary(
                "workspace:locate",
                (),
                crate::workspace::locate_current,
            );
        }

        /// A double-clicked tree row.
        #[unsafe(method(codeppWorkspaceActivate:))]
        fn workspace_activate(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary(
                "workspace:activate",
                (),
                crate::workspace::activate_selected_row,
            );
        }

        // The context menu. Each item carries its row's node id in its
        // `tag`, never a row index — a row index would address a
        // different file if the tree changed between opening the menu and
        // clicking it, which is the same class of bug DESIGN.md §7.4
        // records for the Win32 tab strip's arm/commit race.
        #[unsafe(method(codeppWorkspaceOpen:))]
        fn workspace_open(&self, sender: Option<&NSMenuItem>) {
            workspace_command(sender, crate::workspace::Command::Open);
        }

        #[unsafe(method(codeppWorkspaceCopyPath:))]
        fn workspace_copy_path(&self, sender: Option<&NSMenuItem>) {
            workspace_command(sender, crate::workspace::Command::CopyPath);
        }

        #[unsafe(method(codeppWorkspaceCopyName:))]
        fn workspace_copy_name(&self, sender: Option<&NSMenuItem>) {
            workspace_command(sender, crate::workspace::Command::CopyName);
        }

        #[unsafe(method(codeppWorkspaceRunBySystem:))]
        fn workspace_run_by_system(&self, sender: Option<&NSMenuItem>) {
            workspace_command(sender, crate::workspace::Command::RunBySystem);
        }

        #[unsafe(method(codeppWorkspaceRevealInFinder:))]
        fn workspace_reveal(&self, sender: Option<&NSMenuItem>) {
            workspace_command(sender, crate::workspace::Command::RevealInFinder);
        }

        #[unsafe(method(codeppWorkspaceFindInFiles:))]
        fn workspace_find_in_files(&self, sender: Option<&NSMenuItem>) {
            workspace_command(sender, crate::workspace::Command::FindInFiles);
        }

        #[unsafe(method(codeppWorkspaceRemoveRoot:))]
        fn workspace_remove_root(&self, sender: Option<&NSMenuItem>) {
            workspace_command(sender, crate::workspace::Command::RemoveRoot);
        }

        // ---- m4a: search -------------------------------------------

        #[unsafe(method(codeppShowFind:))]
        fn show_find(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:showFind", (), crate::search::show_find);
        }

        #[unsafe(method(codeppShowReplace:))]
        fn show_replace(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:showReplace", (), crate::search::show_replace);
        }

        #[unsafe(method(codeppShowGoto:))]
        fn show_goto(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:showGoto", (), crate::search::show_goto);
        }

        #[unsafe(method(codeppFindNext:))]
        fn find_next(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("search:findNext", (), || crate::search::do_find(true));
        }

        #[unsafe(method(codeppFindPrevious:))]
        fn find_previous(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("search:findPrev", (), || crate::search::do_find(false));
        }

        /// Find Next from the *menu*, which must work with the panel
        /// closed — so it repeats the shell's remembered query rather than
        /// reading the panel's field. See `search::find_next_repeat`.
        #[unsafe(method(codeppFindNextRepeat:))]
        fn find_next_repeat(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:findNext", (), crate::search::find_next_repeat);
        }

        #[unsafe(method(codeppFindPrevRepeat:))]
        fn find_prev_repeat(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:findPrev", (), crate::search::find_prev_repeat);
        }

        #[unsafe(method(codeppCount:))]
        fn count_matches(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("search:count", (), crate::search::do_count);
        }

        #[unsafe(method(codeppReplace:))]
        fn replace_one(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("search:replace", (), crate::search::do_replace_one);
        }

        #[unsafe(method(codeppReplaceAll:))]
        fn replace_all(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("search:replaceAll", (), crate::search::do_replace_all);
        }

        // ---- m4b: find in files ------------------------------------

        #[unsafe(method(codeppShowFindInFiles:))]
        fn show_find_in_files(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary(
                "menu:showFindInFiles",
                (),
                crate::search::show_find_in_files,
            );
        }

        #[unsafe(method(codeppFifFindAll:))]
        fn fif_find_all(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("fif:findAll", (), crate::search::do_find_all);
        }

        #[unsafe(method(codeppFifReplaceInFiles:))]
        fn fif_replace_in_files(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary(
                "fif:replaceInFiles",
                (),
                crate::search::do_replace_in_files,
            );
        }

        #[unsafe(method(codeppFifBrowse:))]
        fn fif_browse(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("fif:browse", (), crate::search::browse_fif_directory);
        }

        #[unsafe(method(codeppFifCancel:))]
        fn fif_cancel(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("fif:cancel", (), crate::fif::cancel_job);
        }

        #[unsafe(method(codeppFifClose:))]
        fn fif_close(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("fif:close", (), crate::fif::close_dock);
        }

        /// A results row was double-clicked.
        ///
        /// The row index comes from the *sender* rather than from the
        /// table's selection: `clickedRow` is what the double-click
        /// actually hit, and it is `-1` for a click on the header or on
        /// empty space below the last row, which the handler rejects.
        #[unsafe(method(codeppFifRowActivated:))]
        fn fif_row_activated(&self, sender: Option<&NSTableView>) {
            crate::at_callback_boundary("fif:rowActivated", (), || {
                if let Some(table) = sender {
                    crate::fif::row_activated(table.clickedRow());
                }
            });
        }

        /// The tab strip's overflow arrows.
        #[unsafe(method(codeppScrollTabsLeft:))]
        fn scroll_tabs_left(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("tabs:scrollLeft", (), || crate::scroll_tabs(false));
        }

        #[unsafe(method(codeppScrollTabsRight:))]
        fn scroll_tabs_right(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("tabs:scrollRight", (), || crate::scroll_tabs(true));
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

        /// Reveal the `userDefineLangs` folder in Finder, so a user can
        /// drop a `.udl.xml` in without leaving the app.
        #[unsafe(method(codeppOpenUdlFolder:))]
        fn open_udl_folder(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:openUdlFolder", (), open_udl_folder);
        }

        /// Open the N++ community UDL collection in the browser.
        #[unsafe(method(codeppOpenUdlCollection:))]
        fn open_udl_collection(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:openUdlCollection", (), || {
                let Some(url) = NSURL::URLWithString(&NSString::from_str(UDL_COLLECTION_URL)) else {
                    return;
                };
                NSWorkspace::sharedWorkspace().openURL(&url);
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

        #[unsafe(method(codeppAbout:))]
        fn about(&self, _sender: Option<&NSObject>) {
            crate::at_callback_boundary("menu:about", (), || show_about(self.mtm()));
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

/// Run one workspace context-menu command, reading the row's node id out
/// of the sender's tag.
///
/// A **node id**, not a row index: ids are allocated monotonically and
/// never reused, so a tree that changed between opening the menu and
/// clicking it resolves to "gone" rather than to a different file. The
/// same rule the tab strip follows, and for the same reason — DESIGN.md
/// §7.4 records the index-keyed version of this bug on Win32.
fn workspace_command(sender: Option<&NSMenuItem>, command: crate::workspace::Command) {
    crate::at_callback_boundary("workspace:contextCommand", (), || {
        if let Some(sender) = sender {
            if let Ok(id) = u64::try_from(sender.tag()) {
                crate::workspace::context_command(command, id);
            }
        }
    });
}

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
    } else if action == sel!(codeppToggleWorkspace:) {
        item.setState(isize::from(crate::workspace::is_visible()));
    } else if action == sel!(codeppToggleDocMap:) {
        // A panel rather than a persisted editor setting, so it carries
        // no `VIEW_TOGGLES` tag and is matched on its selector. The mark
        // still comes from live state, like every other one here.
        item.setState(isize::from(crate::docmap::is_visible()));
    }
    true
}

/// Reveal the `userDefineLangs` folder in Finder.
///
/// `create_dir_all` first — matching `ui_gtk` and Win32 — so a click
/// that races a between-boots deletion still targets a valid path
/// rather than silently doing nothing.
fn open_udl_folder() {
    let Some(dir) = codepp_platform::user_define_langs_dir() else {
        tracing::warn!("no config dir; cannot open the User Defined Language folder");
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(?e, "could not create the User Defined Language folder");
        return;
    }
    let Some(path) = dir.to_str() else {
        tracing::warn!(?dir, "UDL folder path is not valid UTF-8");
        return;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    NSWorkspace::sharedWorkspace().openURL(&url);
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
    main_menu.addItem(&build_app_menu(actions, mtm));
    main_menu.addItem(&build_file_menu(actions, mtm));
    main_menu.addItem(&build_edit_menu(mtm));
    main_menu.addItem(&build_search_menu(actions, mtm));
    main_menu.addItem(&build_view_menu(actions, mtm));
    main_menu.addItem(&build_encoding_menu(actions, mtm));
    main_menu.addItem(&build_language_menu(actions, mtm));
    main_menu.addItem(&build_window_menu(actions, mtm));
    main_menu.addItem(&build_help_menu(actions, mtm));
    app.setMainMenu(Some(&main_menu));
    debug_assert!(
        duplicate_key_equivalent(&main_menu).is_none(),
        "two menu items share a key equivalent: {:?} — the earlier menu wins and \
         the later item's shortcut is dead",
        duplicate_key_equivalent(&main_menu)
    );
    main_menu
}

/// The application menu.
///
/// macOS mandates this menu and gives it the process name. The first
/// item of the main menu is *always* treated as the application menu
/// regardless of its title, so the title here is never displayed.
fn build_app_menu(actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    add(
        &app_menu,
        mtm,
        "About Code++",
        sel!(codeppAbout:),
        "",
        Some(actions),
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
    // Notepad++'s own File-menu entry, and the discoverable way in to the
    // workspace panel — the View toggle and the toolbar button both route
    // to the same picker when no root is set, but neither reads as "open
    // a folder".
    add(
        &file_menu,
        mtm,
        "Open Folder as Workspace…",
        sel!(codeppOpenFolderAsWorkspace:),
        "",
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
fn build_search_menu(actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let (item, menu) = submenu("Search", mtm);
    add(
        &menu,
        mtm,
        "Find…",
        sel!(codeppShowFind:),
        "f",
        Some(actions),
    );
    // **⌥⌘F, not Notepad++'s Ctrl+H.** ⌘H is taken by "Hide Code++" in
    // the application menu, which macOS mandates — and the App menu is
    // installed first, so `performKeyEquivalent:` finds Hide and never
    // reaches here: the shortcut would be advertised in the menu and
    // silently do nothing but hide the app. ⌥⌘F is what macOS editors use
    // for find-and-replace, so it is both free and the expected chord.
    add_with_modifiers(
        &menu,
        mtm,
        "Replace…",
        sel!(codeppShowReplace:),
        "f",
        NSEventModifierFlags::Command | NSEventModifierFlags::Option,
        Some(actions),
    );
    // ⇧⌘F — the chord macOS editors use for a project-wide search, and
    // free here (⌘F is Find, ⌥⌘F is Replace).
    add_with_modifiers(
        &menu,
        mtm,
        "Find in Files…",
        sel!(codeppShowFindInFiles:),
        "f",
        NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
        Some(actions),
    );
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    // ⌘G / ⇧⌘G — the macOS convention for find-again, in place of
    // Notepad++'s F3. These repeat the *shell's* query so they work with
    // the panel closed.
    add(
        &menu,
        mtm,
        "Find Next",
        sel!(codeppFindNextRepeat:),
        "g",
        Some(actions),
    );
    add(
        &menu,
        mtm,
        "Find Previous",
        sel!(codeppFindPrevRepeat:),
        "G",
        Some(actions),
    );
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    // ⌘L, not Notepad++'s Ctrl+G: ⌘G is taken by find-again on this
    // platform, and ⌘L is what macOS editors use for go-to-line.
    add(
        &menu,
        mtm,
        "Go to…",
        sel!(codeppShowGoto:),
        "l",
        Some(actions),
    );
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
    // The Document Map's own check mark is resolved in
    // `validateMenuItem:` like every other mark in this backend, but from
    // the panel's live visibility rather than from `VIEW_TOGGLES` — it is
    // a panel, not a persisted editor setting, so it carries no tag and
    // is matched on its selector.
    add(
        &menu,
        mtm,
        "Folder as Workspace",
        sel!(codeppToggleWorkspace:),
        "",
        Some(actions),
    );
    add(
        &menu,
        mtm,
        "Document Map",
        sel!(codeppToggleDocMap:),
        "",
        Some(actions),
    );
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
    let (item, menu) = submenu(LANGUAGE_MENU_TITLE, mtm);
    // The letter groups are marked on open rather than at build time: the
    // active language changes with every tab switch, and a submenu item
    // is not something `validateMenuItem:` is asked about. See
    // [`mark_language_letter_groups`].
    //
    // SAFETY: as for the Window menu — `Actions` implements
    // `NSMenuDelegate` and AppKit's delegate reference is weak, while the
    // window state owns `actions` for the process lifetime.
    menu.setDelegate(Some(ProtocolObject::from_ref(actions)));
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

    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&build_udl_submenu(actions, mtm));
    // Everything past here is regenerated on open — see
    // [`rebuild_udl_rows`]. Captured rather than hardcoded because the
    // prefix is ~90 items whose count depends on how `LANG_TABLE`'s
    // labels happen to group into letters; a constant would drift the
    // first time a language is added, and drift here means the rebuild
    // eats built-in rows.
    LANGUAGE_MENU_FIXED_ITEMS.with(|c| c.set(menu.numberOfItems()));

    item
}

/// The N++ community UDL collection. Compile-time constant — no user- or
/// UDL-supplied string ever reaches `NSWorkspace`, the same property
/// [`open_help_url`] holds.
const UDL_COLLECTION_URL: &str = "https://github.com/notepad-plus-plus/userDefinedLanguages";

thread_local! {
    /// Number of items [`build_language_menu`] leaves behind, i.e. the
    /// index at which [`rebuild_udl_rows`] starts replacing. Zero until
    /// the menu is built, which makes the rebuild a no-op rather than a
    /// truncation of a menu it does not understand.
    static LANGUAGE_MENU_FIXED_ITEMS: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// The "User-Defined language" submenu at the bottom of the Language
/// menu, matching `ui_gtk`'s and Notepad++'s.
///
/// "Define your language…" is greyed: the UDL editor modal (Phase 4.6
/// m3) exists only on Win32. It carries no action rather than an
/// explicit `setEnabled(false)`, which is what AppKit's automatic menu
/// enabling greys for free — see [`add_disabled`].
fn build_udl_submenu(actions: &Actions, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let (item, sub) = submenu("User-Defined language", mtm);
    add_disabled(&sub, mtm, "Define your language…");
    add(
        &sub,
        mtm,
        "Open User Defined Language folder…",
        sel!(codeppOpenUdlFolder:),
        "",
        Some(actions),
    );
    add(
        &sub,
        mtm,
        "Notepad++ User Defined Languages Collection",
        sel!(codeppOpenUdlCollection:),
        "",
        Some(actions),
    );
    item
}

/// Replace the Language menu's loaded-UDL rows.
///
/// They live **flat at the top level**, below the "User-Defined
/// language" submenu after a separator, which is where Notepad++ puts
/// them — so applying a UDL is one hover-and-click rather than two.
///
/// Rebuilt on open rather than at build time for a structural reason:
/// `build_language_menu` runs *before* `Shell::new`, so no registry
/// exists yet when the menu is constructed. Same shape as
/// [`rebuild_window_menu`], including its refusal to rebuild when the
/// state borrow is declined.
fn rebuild_udl_rows(menu: &NSMenu, actions: &Actions) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let fixed = LANGUAGE_MENU_FIXED_ITEMS.with(std::cell::Cell::get);
    if fixed == 0 {
        // The menu was never built by us. Touching it would truncate
        // something we do not understand.
        return;
    }
    let Some(rows) = crate::udl::language_rows() else {
        // Re-entrant borrow, not "no UDLs installed" — leave the rows
        // that are already there. See `udl::language_rows`.
        return;
    };
    while menu.numberOfItems() > fixed {
        menu.removeItemAtIndex(menu.numberOfItems() - 1);
    }
    if rows.is_empty() {
        // No separator either: a lone trailing separator under the
        // submenu reads as a menu with something missing.
        return;
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    for (lang_id, name) in rows {
        add_language(menu, &name, lang_id, actions, mtm);
    }
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

/// Title identifying the Language menu to the shared menu delegate.
const LANGUAGE_MENU_TITLE: &str = "Language";

/// True if `menu` is the one [`build_language_menu`] built.
fn is_language_menu(menu: &NSMenu) -> bool {
    menu.title().to_string() == LANGUAGE_MENU_TITLE
}

/// Mark the lettered group that contains the active language.
///
/// **Why this is needed at all.** The leaf language rows get their radio
/// mark from `validateMenuItem:`, but ~80 of them live inside a lettered
/// submenu, and a closed submenu shows nothing — so a buffer detected as
/// C++ gave the user no way to see that without opening "C" and looking.
/// Marking the group itself is the hint.
///
/// **Why on menu-open rather than from `validateMenuItem:`.** AppKit does
/// not validate an item that owns a submenu: validation resolves an
/// item's *action* against the responder chain, and a submenu item has no
/// action. So the mark has to be pushed, and menu-open is the point at
/// which it is both cheap and guaranteed current.
///
/// The mark is cleared from every group first, so a language change that
/// moves the active buffer from one letter to another leaves no stale
/// tick behind. Only items that own a submenu are touched; the leaf rows
/// at the top level (Normal Text and the single-language letters) keep
/// the mark `validateMenuItem:` gives them.
fn mark_language_letter_groups(menu: &NSMenu) {
    let Some(active) = crate::active_lang_id() else {
        // Unreadable state, or no active buffer. Leave the marks alone
        // rather than clearing them from a read that never happened —
        // the same distinction `open_buffer_rows` draws.
        return;
    };
    for item in &menu.itemArray() {
        let Some(sub) = item.submenu() else {
            continue;
        };
        // Matched on the *action* as well as the tag, not the tag alone.
        // A tag defaults to 0, which is `L_TEXT`'s own id, so a submenu
        // whose children are not language rows — the "User-Defined
        // language" submenu, whose three items carry no tag — would be
        // marked for every buffer whose language is Normal Text, i.e.
        // every untitled one. Measured on the real app before this
        // clause existed: `active_lang=Some(0)` and the UDL submenu came
        // up ticked. Checking the selector states the actual intent
        // ("the group holding the active language row") and stays
        // correct if another non-language submenu is ever added here.
        let contains = sub.itemArray().iter().any(|child| {
            child.tag() == active as isize && child.action() == Some(sel!(codeppSetLanguage:))
        });
        item.setState(isize::from(contains));
    }
}

/// The application icon, embedded rather than read from disk.
///
/// Needed because there is no `.app` bundle for AppKit to take an icon
/// from, so the About panel falls back to a generic document icon —
/// measured, not assumed: the first version of this panel rendered a blue
/// folder. The same source PNG `ui_gtk` loads and `ui_win32` compiles
/// into `code++.ico`, so all three show one icon. §9.2 schedules a real
/// bundle for macOS distribution; this keeps `cargo run` — the documented
/// development workflow — looking right in the meantime.
const PNG_APP_ICON: &[u8] = include_bytes!("../../../assets/code++.png");

/// Logical edge length the About panel's icon is drawn at. AppKit's panel
/// reserves a 64-point square; the source art is far larger, so pinning
/// the logical size lets it downsample rather than overflow.
const ABOUT_ICON_PX: f64 = 64.0;

/// Architecture suffix for the About panel, matching `ui_win32`'s
/// `ABOUT_ARCH`. Compile-time, so the two backends report the same thing
/// for the same build.
///
/// Passed as `NSAboutPanelOptionVersion`, which AppKit documents as the
/// *build* version and renders parenthesised after the marketing
/// version. Using it for the architecture is deliberately off-label: it
/// puts the panel's first line at "Version 0.1.0 (64-bit)", which is the
/// same shape `ui_win32`'s About title has ("Code++ v0.1.0  (64-bit)"),
/// and Code++ has no build number to put there instead. If one is ever
/// introduced, it takes this key and the arch moves into the credits
/// block.
const ABOUT_ARCH: &str = if cfg!(target_pointer_width = "64") {
    "64-bit"
} else {
    "32-bit"
};

/// The one-line description, verbatim from `ui_gtk`'s About dialog so the
/// two do not drift.
const ABOUT_SUMMARY: &str = "A fast, cross-platform code and text editor built on Scintilla.";

/// Copyright and licence line. The full MIT text is what `ui_win32`'s
/// About shows in a group box; here it is named rather than reproduced,
/// because macOS's About panel is a compact one and `LICENSE` at the repo
/// root is the authoritative copy either way.
const ABOUT_COPYRIGHT: &str =
    "Copyright (c) 2026 Max Fiedler and Code++ contributors. MIT licensed.";

/// Show the About panel.
///
/// Uses AppKit's own About panel with our content substituted, rather
/// than a hand-built window. That is the same call the m1 convention
/// decision makes for every other App-menu item: About is one of the
/// three macOS *mandates* there, so the panel should look like the one
/// users get from every other application — while carrying the
/// information `ui_win32` and `ui_gtk` show, which is what this exists to
/// fix. The bare `orderFrontStandardAboutPanel:` it replaces showed only
/// a process name and nothing else, because there is no `.app` bundle
/// `Info.plist` for AppKit to read any of this out of.
///
/// The credits block carries what does not fit the panel's fixed fields:
/// the summary line, the home page, and the upstream acknowledgement
/// DESIGN.md's own README considers due.
fn show_about(mtm: MainThreadMarker) {
    let credits = format!(
        "{ABOUT_SUMMARY}\n\n{}\n\nBuilt on Scintilla and Lexilla by Neil Hodgson and \
         contributors.\n\n{ABOUT_COPYRIGHT}",
        HELP_URLS[0].1
    );
    let credits = NSAttributedString::initWithString(
        NSAttributedString::alloc(),
        &NSString::from_str(&credits),
    );
    let version = NSString::from_str(env!("CARGO_PKG_VERSION"));
    let arch = NSString::from_str(ABOUT_ARCH);
    let name = NSString::from_str("Code++");

    // The icon is optional: a decode failure costs the panel its artwork
    // and nothing else, which is the same way the tab strip degrades.
    let icon = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(PNG_APP_ICON));
    if let Some(icon) = icon.as_deref() {
        icon.setSize(NSSize::new(ABOUT_ICON_PX, ABOUT_ICON_PX));
    }

    // SAFETY: every key is one of AppKit's own `extern "C"` statics, and
    // every value has the type its key documents — `NSAttributedString`
    // for Credits, `NSImage` for ApplicationIcon, `NSString` for the
    // rest. `AnyObject` is what the dictionary is declared over, so each
    // is upcast to it.
    let (keys, values): (Vec<&NSAboutPanelOptionKey>, Vec<&AnyObject>) = unsafe {
        let mut keys = vec![
            NSAboutPanelOptionApplicationName,
            NSAboutPanelOptionApplicationVersion,
            NSAboutPanelOptionVersion,
            NSAboutPanelOptionCredits,
        ];
        let mut values = vec![
            name.as_ref() as &AnyObject,
            version.as_ref() as &AnyObject,
            arch.as_ref() as &AnyObject,
            credits.as_ref() as &AnyObject,
        ];
        // **The key is omitted, not defaulted, when the icon is missing.**
        // The tempting shortcut is to pass some other object and rely on
        // AppKit ignoring a wrong-typed value, but the header documents
        // only what happens when the key is *absent* (fall back to
        // `imageNamed:`, then to a generic icon) and says nothing about a
        // present-but-wrong value — which could as easily mean an
        // `NSImage`-only selector sent to an `NSString` and an
        // `NSInvalidArgumentException`. Absent is the documented path, so
        // take it rather than assert something unverified.
        if let Some(icon) = icon.as_deref() {
            keys.push(NSAboutPanelOptionApplicationIcon);
            values.push(icon as &AnyObject);
        }
        (keys, values)
    };
    let options = NSDictionary::from_slices::<NSAboutPanelOptionKey>(&keys, &values);
    // SAFETY: main-thread-only AppKit call, proven by `mtm`, with a
    // well-formed options dictionary.
    unsafe {
        NSApplication::sharedApplication(mtm).orderFrontStandardAboutPanelWithOptions(&options);
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

/// The first key equivalent claimed by two different items, if any.
///
/// **Why this is worth asserting.** `performKeyEquivalent:` walks the bar
/// in order and stops at the first match, so a duplicate does not
/// conflict loudly — the later item simply never fires while still
/// advertising the shortcut next to its name. That is invisible until
/// someone presses it and gets the wrong command. It shipped once: ⌘H was
/// given to Replace while the application menu, which macOS mandates and
/// which is installed first, already had it for Hide.
///
/// Walks submenus recursively, so a shortcut buried in the Language
/// menu's letter groups is covered too. Returns the offending key and the
/// two titles, which is what a developer needs to fix it.
fn duplicate_key_equivalent(menu: &NSMenu) -> Option<(String, String, String)> {
    let mut seen: Vec<(String, usize, String)> = Vec::new();
    collect_key_equivalents(menu, &mut seen);
    for (index, (key, mask, title)) in seen.iter().enumerate() {
        if let Some((_, _, other)) = seen[..index].iter().find(|(k, m, _)| k == key && m == mask) {
            return Some((key.clone(), other.clone(), title.clone()));
        }
    }
    None
}

/// Depth-first walk collecting `(key, modifier mask, title)` for every
/// item that claims a key equivalent.
fn collect_key_equivalents(menu: &NSMenu, out: &mut Vec<(String, usize, String)>) {
    for item in &menu.itemArray() {
        if let Some(sub) = item.submenu() {
            collect_key_equivalents(&sub, out);
            continue;
        }
        let key = item.keyEquivalent().to_string();
        if key.is_empty() {
            continue;
        }
        out.push((
            key,
            item.keyEquivalentModifierMask().bits(),
            item.title().to_string(),
        ));
    }
}

/// Append a menu item whose key equivalent needs modifiers other than
/// the implicit ⌘ (or ⇧⌘ via an uppercase letter).
///
/// [`add`] covers the common cases; this exists for chords like ⌥⌘F,
/// where the modifier set has to be stated because there is no spelling
/// of it in the key string.
fn add_with_modifiers(
    menu: &NSMenu,
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key: &str,
    modifiers: NSEventModifierFlags,
    target: Option<&Actions>,
) {
    let item = add(menu, mtm, title, action, key, target);
    item.setKeyEquivalentModifierMask(modifiers);
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
