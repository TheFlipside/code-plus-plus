//! Window state and the `(&mut Shell, CocoaUi)` split.
//!
//! # Why the split exists
//!
//! `Shell::drain` takes `&mut self` *and* `&mut impl UiPlatform`. Since
//! one struct owns both the `Shell` and the views the trait methods
//! touch, handing both out at once would alias `&mut self`.
//! [`CocoaUiState::split`] resolves that the same way `ui_win32`'s
//! `WindowState::split` and `ui_gtk`'s `GtkUiState::split` do: it hands
//! back a `&mut Shell` borrow plus a freshly-built, cheap [`CocoaUi`]
//! value. Building a new one per call is deliberate and costs almost
//! nothing — `EditorHandle` is `Copy` and `Retained<T>` is a refcount
//! bump, not an object copy.
//!
//! # Why a thread-local
//!
//! Worker threads wake the UI through a closure that carries no data
//! (DESIGN.md §5.4), so the wake handler needs some way to find the
//! state once it is back on the main thread. Win32 stashes a pointer in
//! `GWLP_USERDATA`; GTK uses a main-thread `thread_local`; Cocoa does
//! the same as GTK, because a `dispatch_async` block has no more of a
//! per-window slot to recover than a GTK idle callback does.
//!
//! The `Rc<RefCell<…>>` is never sent anywhere: [`install`] runs on the
//! main thread during startup, and [`with_state`] finds nothing when
//! called from any other thread — a `thread_local` on a worker is simply
//! unset. That is also why the state may hold `Retained<T>`, which is
//! deliberately not `Send`.

use std::cell::RefCell;
use std::rc::Rc;

use codepp_editor::EditorHandle;
use codepp_shell::Shell;
use objc2::rc::Retained;
use objc2_app_kit::{NSMenu, NSView, NSWindow};

use crate::fif::{FifDock, FifJob};
use crate::menu::Actions;
use crate::status::StatusBar;
use crate::tabs::TabStrip;
use crate::toolbar::Toolbar;

/// Everything the Cocoa backend owns for the lifetime of the window.
pub struct CocoaUiState {
    /// The toplevel. Kept so `Shell`-driven operations can retitle it
    /// and so geometry can be read back at save time.
    pub window: Retained<NSWindow>,
    /// The Scintilla view, as an `NSView`.
    ///
    /// Its load-bearing job is to hold a reference for the whole
    /// session. [`Self::editor`] carries raw pointers into the same
    /// view, so dropping this field would leave `EditorHandle`'s
    /// `direct_ptr` dangling on the next keystroke. See the safety note
    /// on `EditorHandle::from_cocoa_view`.
    ///
    /// **The keep-alive is the whole of how this backend satisfies that
    /// note.** `EditorHandle` is `Copy` with no lifetime, so the
    /// compiler will not catch a copy outliving the view; the guarantee
    /// is structural — the view is created once in `run`, never
    /// destroyed, removed from its superview, or reassigned, and tabs
    /// get their own buffers through `SCI_SETDOCPOINTER` rather than
    /// through their own views. On this backend it is enforced one level
    /// lower still: `ScintillaCocoaShim.mm` exposes no release entry
    /// point, so there is no supported way to finalise a view at all.
    /// DESIGN.md §7.4 names an `NSView`-per-tab design as the specific
    /// mistake this avoids.
    ///
    /// `dead_code` is allowed deliberately: **not being read is the
    /// point.** The field's entire job is to hold a reference, so a
    /// future reader who "cleans up the unused field" would silently
    /// reintroduce the dangling-`direct_ptr` bug. Same allow, for the
    /// same reason, as `ui_gtk`'s `docmap_sci`.
    #[allow(dead_code)]
    pub sci_view: Retained<NSView>,
    /// Raw `ScintillaView*` (the `scintilla_cocoa_new()` return). Will be
    /// handed to plugins as `NppData._scintillaMainHandle` once the
    /// plugin host lands; kept here so that does not need a second
    /// capture, which would mean a second `scintilla_cocoa_new` and
    /// therefore a second view.
    #[allow(dead_code)]
    pub sci_ptr: *mut std::ffi::c_void,
    /// Direct-call handle for the one Scintilla view. m2 is
    /// single-view: tabs switch documents under it via
    /// `SCI_SETDOCPOINTER`, exactly as the other two backends do.
    pub editor: EditorHandle,
    /// The 7-part status bar.
    pub status: StatusBar,
    /// The tab strip. Purely a selector — the one Scintilla view is its
    /// sibling, not its child. See [`crate::tabs`].
    pub tabs: TabStrip,
    /// The toolbar. Held so its three functional toggles can be repainted
    /// when a View setting is changed from the menu instead of from here.
    pub toolbar: Toolbar,
    /// The Objective-C receiver for menu and tab-strip actions. Held
    /// here because `NSMenuItem.target` and `NSControl.target` are both
    /// *weak*, so nothing else keeps it alive — and the tab strip
    /// rebuilds its buttons against it on every sync.
    pub actions: Retained<Actions>,
    /// The application's main menu, held so the menu-hide toggle can
    /// reach it. Cocoa has no "hide the menu bar" for an ordinary app
    /// the way Win32 does — see `platform.rs`'s `set_menu_hidden`.
    pub menu: Retained<NSMenu>,
    /// The Find-in-Files results dock, along the bottom above the status
    /// bar. Built at startup and hidden until a search produces results,
    /// so a session that never searches pays only its construction.
    pub fif_dock: FifDock,
    /// Per-job Find-in-Files runtime — counters, buffered matches, the
    /// queued jump. Separate from [`Self::fif_dock`] because that one is
    /// `Clone` and handed out on every split; this is single-owner state.
    pub fif_job: FifJob,
    /// The modeless Find/Replace panel, built on first use and then kept
    /// for the session — its controls are read by every search command,
    /// so they have to outlive the click that opened it. `None` until
    /// the user first asks for Find or Replace, so a session that never
    /// searches never builds it.
    pub find_replace: Option<crate::search::FindReplaceDialog>,
    /// Headless session/file/watcher logic. Owns the tab list.
    pub shell: Shell,
}

/// The `UiPlatform` implementor. Cheap to build; see the module docs.
///
/// Deliberately a value rather than a borrow of [`CocoaUiState`]: that
/// is the whole point of [`CocoaUiState::split`].
pub struct CocoaUi {
    pub window: Retained<NSWindow>,
    /// The Scintilla view, so [`CocoaUi::relayout_chrome`] can resize it.
    ///
    /// **Why the view and not just the handle.** Hiding a chrome strip has
    /// to give the freed band back to the editor, and that is a *frame*
    /// change on the view — `EditorHandle` addresses the Scintilla
    /// document, not the `NSView`. It has to live here rather than be
    /// fetched through `with_state`, because every trait method runs
    /// *inside* a `with_state` borrow already and a nested one is declined.
    pub sci_view: Retained<NSView>,
    pub editor: EditorHandle,
    pub status: StatusBar,
    pub menu: Retained<NSMenu>,
    pub tabs: TabStrip,
    pub toolbar: Toolbar,
    /// The results dock, so [`CocoaUi::relayout_chrome`] can give it its
    /// band and take it back when it is closed.
    pub fif_dock: FifDock,
}

impl CocoaUiState {
    /// Split into a `(shell, ui-platform)` pair so `shell.drain(ui)` can
    /// be called without aliasing `&mut self`.
    pub fn split(&mut self) -> (&mut Shell, CocoaUi) {
        let ui = CocoaUi {
            window: self.window.clone(),
            sci_view: self.sci_view.clone(),
            editor: self.editor,
            status: self.status.clone(),
            menu: self.menu.clone(),
            tabs: self.tabs.clone(),
            toolbar: self.toolbar.clone(),
            fif_dock: self.fif_dock.clone(),
        };
        (&mut self.shell, ui)
    }
}

thread_local! {
    /// Set once on the main thread at startup by [`install`].
    static STATE: RefCell<Option<Rc<RefCell<CocoaUiState>>>> = const { RefCell::new(None) };
}

/// Publish `state` so [`with_state`] can find it. Main thread only.
pub fn install(state: &Rc<RefCell<CocoaUiState>>) {
    STATE.with(|s| *s.borrow_mut() = Some(Rc::clone(state)));
}

/// Run `f` against the window state, if it is installed and reachable.
///
/// Returns `None` — rather than panicking — in three cases, because
/// every one of them is reachable during normal operation and none is a
/// bug. The list matches `ui_gtk`'s for the same reasons:
///
/// * called from a thread with no state installed (a worker's wake
///   already hopped to the main thread, but a stray direct call must not
///   take the process down);
/// * called after the window is gone, when startup failed partway or
///   teardown has run;
/// * called re-entrantly, while an outer `with_state` still holds the
///   `RefCell`. That last one is the real hazard: an AppKit action can
///   fire *inside* a Scintilla call made from another handler, and
///   `borrow_mut` would panic. Skipping the inner call is correct
///   because the outer one is already mid-update.
pub fn with_state<R>(f: impl FnOnce(&mut CocoaUiState) -> R) -> Option<R> {
    let state = STATE.with(|s| s.borrow().clone())?;
    // Distinguish the three `None` cases in the log. A re-entrant skip
    // is the one that could hide a dropped user action if a future call
    // site stops being self-settling, so it must leave a trail rather
    // than vanishing silently.
    let Ok(mut guard) = state.try_borrow_mut() else {
        tracing::debug!("with_state skipped: re-entrant call while an outer borrow was live");
        return None;
    };
    Some(f(&mut guard))
}

/// Drop the installed state, so the `Shell` — and the worker threads its
/// channels keep alive — tear down deterministically.
///
/// Unlike GTK, where this runs after `gtk::main()` returns, on macOS the
/// standard Quit path never returns from `-[NSApplication run]`
/// (`terminate:` calls `exit()`), so this is driven from the
/// application delegate's `applicationWillTerminate:` instead. See
/// [`crate::delegate`].
pub fn uninstall() {
    STATE.with(|s| *s.borrow_mut() = None);
}
