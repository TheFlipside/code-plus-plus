//! Window state and the `(&mut Shell, GtkUi)` split.
//!
//! # Why the split exists
//!
//! [`Shell::drain`] takes `&mut self` *and* `&mut impl UiPlatform`.
//! Since one struct owns both the `Shell` and the widgets the trait
//! methods touch, handing both out at once would alias `&mut self`.
//! [`GtkUiState::split`] resolves that the same way `ui_win32`'s
//! `WindowState::split` does: it hands back a `&mut Shell` borrow plus
//! a freshly-built, cheap `GtkUi` value. Building a new one per call is
//! deliberate and costs almost nothing — `EditorHandle` is `Copy` and
//! gtk-rs widgets are refcounted handles, so a clone is a refcount
//! bump, not a widget copy.
//!
//! # Why a thread-local
//!
//! Worker threads wake the UI through a closure that carries no data
//! (DESIGN.md §5.4), so the wake handler needs some way to find the
//! state once it is back on the main thread. Win32 stashes a pointer in
//! `GWLP_USERDATA` and recovers it in the window procedure; GTK has no
//! equivalent per-window slot that survives into a plain idle callback,
//! so the state lives in a main-thread `thread_local` instead. The
//! `Rc<RefCell<…>>` is never sent anywhere: [`install`] is called on
//! the main thread during startup, and [`with_state`] refuses to do
//! anything if called from any other thread.

use std::cell::RefCell;
use std::rc::Rc;

use codepp_editor::EditorHandle;
use codepp_shell::Shell;

use crate::status::StatusBar;
use crate::tabs::TabStrip;

/// Everything the GTK backend owns for the lifetime of the window.
pub struct GtkUiState {
    /// The toplevel. Kept so `Shell`-driven operations can retitle it
    /// and so the delete-event handler can end the main loop.
    pub window: gtk::Window,
    /// The Scintilla widget, as adopted into gtk-rs.
    ///
    /// Never read, and that is the point: this field exists purely to
    /// hold a `GObject` reference for the whole session. [`Self::editor`]
    /// carries raw pointers into the same widget, and nothing else in
    /// the program owns it once `run`'s local goes out of scope — so
    /// dropping this field would finalise the widget and leave
    /// `EditorHandle`'s `direct_ptr` dangling on the next keystroke.
    /// See the safety note on `EditorHandle::from_gtk_widget`.
    ///
    /// **This is the whole of how the backend satisfies that note.**
    /// `EditorHandle` is `Copy` with no lifetime, so the compiler will
    /// not catch a copy outliving the widget; the guarantee is instead
    /// structural — the widget is created once in `run`, never
    /// destroyed, removed from its container, or reassigned, and tabs
    /// get their own buffers through `SCI_SETDOCPOINTER` rather than
    /// through their own views. Anything that closes over a tab by
    /// destroying a Scintilla widget breaks it. DESIGN.md §7.4 tracked
    /// this as an open ownership question until the tab strip landed
    /// and settled it this way.
    #[allow(dead_code)]
    pub sci_widget: gtk::Widget,
    /// Raw `ScintillaObject*` (the `scintilla_new()` return). Handed to
    /// plugins as `NppData._scintillaMainHandle` and used as the sink for
    /// `scintilla_send_message` when a plugin routes an `SCI_*` message —
    /// the GTK analogue of the real Scintilla `HWND` a Win32 plugin holds.
    pub sci_ptr: *mut std::ffi::c_void,
    /// Direct-call handle for the one Scintilla view. m2 is
    /// single-view: tabs switch documents under it via
    /// `SCI_SETDOCPOINTER`, exactly as Win32 does.
    pub editor: EditorHandle,
    /// The Document Map's miniature Scintilla widget, held for the same
    /// reason as [`Self::sci_widget`]: a `GObject` reference for the whole
    /// session so [`Self::docmap_editor`]'s raw pointers never dangle.
    /// Created once, never destroyed or reassigned — the same discipline
    /// that lets [`Self::sci_widget`] be safe. See [`crate::docmap`].
    #[allow(dead_code)]
    pub docmap_sci: gtk::Widget,
    /// Direct-call handle for the Document Map's miniature view. Shares
    /// each tab's document via `SCI_SETDOCPOINTER`; see [`crate::docmap`].
    pub docmap_editor: EditorHandle,
    /// The 7-part status bar.
    pub status: StatusBar,
    /// Menu bar, held so the visibility toggles can reach it.
    pub menu_bar: gtk::MenuBar,
    /// The toolbar, held so its visibility toggle (`NPPM_HIDETOOLBAR`) and
    /// the split's `is_/set_toolbar_hidden` can reach the live widget.
    pub toolbar: gtk::Toolbar,
    /// The tab strip. Purely a selector — the one Scintilla view is
    /// its sibling, not its child. See `crate::tabs`.
    pub tabs: TabStrip,
    /// Headless session/file/watcher logic. Owns the tab list.
    pub shell: Shell,
    /// The modeless Find/Replace dialog, once opened.
    ///
    /// Modeless per Notepad++: it stays up while the user reads results
    /// and edits between clicks, so it must outlive the handler that
    /// opened it — hence a field rather than a local. Reopening reuses
    /// this rather than stacking a second dialog. `None` until the
    /// user first invokes Find or Replace.
    pub find_replace: Option<crate::search::FindReplaceDialog>,
    /// The Find-in-Files results dock. Built once at startup and hidden
    /// until a search produces results; lives in the lower pane of the
    /// editor/dock splitter. See [`crate::fif`].
    pub fif_dock: crate::fif::FifDock,
    /// The "Folder as Workspace" side panel: a lazily-populated directory
    /// tree in the left pane of a horizontal splitter that wraps the
    /// editor column. Hidden until a folder is opened. See
    /// [`crate::workspace`].
    ///
    /// Not part of [`GtkUi`]: no `UiPlatform` method touches it, so it is
    /// reached only through [`with_state`] and stays out of the split.
    pub workspace: crate::workspace::WorkspacePanel,
    /// The right-side "Document Map" panel: a zoomed-out miniature of the
    /// active buffer with a translucent orange viewport box. Hidden until
    /// opened. Reached only through [`with_state`]; see [`crate::docmap`].
    pub docmap: crate::docmap::DocMapPanel,
}

/// The `UiPlatform` implementor. Cheap to build; see the module docs.
///
/// Deliberately a value rather than a borrow of [`GtkUiState`]: that is
/// the whole point of [`GtkUiState::split`].
pub struct GtkUi {
    pub window: gtk::Window,
    pub editor: EditorHandle,
    pub status: StatusBar,
    pub menu_bar: gtk::MenuBar,
    pub toolbar: gtk::Toolbar,
    pub tabs: TabStrip,
    /// Read-only pointer to `Shell.udl_registry` for the UDL
    /// container-lexer path. `apply_lang` runs inside a `drain` (the split
    /// `&mut Shell` borrow is live), so it can't reach the registry through
    /// `with_state`; this raw pointer is the same escape hatch
    /// `Win32Ui.udl_registry` uses.
    ///
    /// **Aliasing discipline** (mirrors `Win32Ui::udl_registry`). Today the
    /// registry is populated once at `Shell::new` and never mutated on GTK
    /// (the UDL editor modal — Phase 4.6 m3 — is not yet ported here), so
    /// every read through this pointer is shared and never races the
    /// `&mut Shell` the split hands out. When the GTK UDL editor *does*
    /// land, its "save UDL → rescan" must mutate `shell.udl_registry`
    /// through `with_state` **only** (no `GtkUi` on the stack), so this
    /// pointer never observes a `&mut UdlRegistry`. Win32 relies on the
    /// same hand-maintained discipline — see its field doc.
    pub udl_registry: *const codepp_udl::UdlRegistry,
}

impl GtkUiState {
    /// Split into a `(shell, ui-platform)` pair so `shell.drain(ui)`
    /// can be called without aliasing `&mut self`.
    pub fn split(&mut self) -> (&mut Shell, GtkUi) {
        // SAFETY of `&raw const`: the pointer is created without forming a
        // reference, so it doesn't conflict with the `&mut self.shell`
        // returned below. It is only dereferenced (read-only) later while a
        // `GtkUi` is in scope, and the registry is never mutated after
        // `Shell::new`. Same pattern as `Win32Ui`'s `udl_registry` capture.
        let udl_registry = &raw const self.shell.udl_registry;
        let ui = GtkUi {
            window: self.window.clone(),
            editor: self.editor,
            status: self.status.clone(),
            menu_bar: self.menu_bar.clone(),
            toolbar: self.toolbar.clone(),
            tabs: self.tabs.clone(),
            udl_registry,
        };
        (&mut self.shell, ui)
    }
}

thread_local! {
    /// Set once on the main thread at startup by [`install`].
    static STATE: RefCell<Option<Rc<RefCell<GtkUiState>>>> = const { RefCell::new(None) };
}

/// Publish `state` so [`with_state`] can find it. Main thread only.
pub fn install(state: &Rc<RefCell<GtkUiState>>) {
    STATE.with(|s| *s.borrow_mut() = Some(Rc::clone(state)));
}

/// Run `f` against the window state, if it is installed and reachable.
///
/// Returns `None` — rather than panicking — in three cases, because
/// every one of them is reachable during normal shutdown and none is a
/// bug:
///
/// * called from a thread with no state installed (a worker's wake
///   already hopped to the main thread, but a stray direct call must
///   not take the process down);
/// * called after the window is gone, when startup failed partway;
/// * called re-entrantly, while an outer `with_state` still holds the
///   `RefCell`. That last one is the real hazard: a GTK signal handler
///   can fire *inside* a Scintilla call made from another handler, and
///   `borrow_mut` would panic. Skipping the inner call is correct here
///   because the outer one is already mid-update.
pub fn with_state<R>(f: impl FnOnce(&mut GtkUiState) -> R) -> Option<R> {
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

/// Drop the installed state. Called as the main loop exits so the
/// `Shell` — and the worker threads its channels keep alive — are torn
/// down deterministically rather than at process teardown.
pub fn uninstall() {
    STATE.with(|s| *s.borrow_mut() = None);
}
