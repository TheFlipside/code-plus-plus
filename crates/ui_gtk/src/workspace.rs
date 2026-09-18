//! The "Folder as Workspace" side panel: a lazily-populated directory
//! tree docked to the left of the editor, mirroring `ui_win32`'s
//! `SysTreeView32`-based panel.
//!
//! # Layout
//!
//! The panel is a *dock panel* (`DockPanel::Workspace`): its content
//! widget — the action bar plus the tree — is built once here and then
//! hosted by [`crate::dock`], which places it in a docked group, a tab
//! of a shared group, or a floating window wherever the user last put
//! it, and parks it hidden otherwise. The caption (title + close ✕) is
//! the group's, not this panel's; so is the resize splitter. Every
//! show/hide here funnels through `dock::set_panel_visible`, after the
//! panel-specific preparation (populate before show, cancel a walk
//! before hide) that the dock model cannot know about.
//!
//! # The value-vs-label split (security)
//!
//! A tree row's **display** text is passed through
//! [`codepp_shell::sanitize_filename_for_display`], so a filename
//! carrying a bidi override or a zero-width char cannot spoof its
//! extension in the panel — the exact trap the Win32 tree closed. The
//! **real** path a double-click or context action operates on is never
//! parsed back out of that display text: it lives out-of-band in
//! [`WorkspacePanel::paths`], keyed by a synthetic per-row id stored in a
//! hidden model column. This is the GTK shape of Win32's
//! `workspace_tree_names` side table — and it doubles as the fix for
//! non-UTF-8 paths, which a `to_string_lossy` round-trip through the
//! model would corrupt.
//!
//! Every functional path is additionally re-checked with
//! `starts_with(root)` before it is opened or launched, defence-in-depth
//! against a hostile filesystem handing back a component with an
//! absolute-path prefix — the same guard the Win32 double-click path
//! keeps.
//!
//! # Lazy population
//!
//! Each folder row is inserted with a single empty placeholder child so
//! GTK draws its expander. The first time the row expands, the
//! placeholder is dropped and the directory is read once
//! (`row-expanded` → [`populate_children`]); the row is then marked
//! populated and never re-read, matching the Win32 `UNPOP`/`POP`
//! sentinel. `read_dir` is bounded to a single directory per expand, so
//! it stays inside the §8 keystroke envelope.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use codepp_core::dock::DockPanel;
use codepp_shell::sanitize_filename_for_display;

use crate::state::with_state;

/// Model columns. Only [`COL_ICON`]/[`COL_NAME`] are shown; the rest are
/// functional state a click reads. [`COL_ID`] keys into
/// [`WorkspacePanel::paths`] for the row's real path.
const COL_ICON: u32 = 0;
const COL_NAME: u32 = 1;
const COL_ID: u32 = 2;
const COL_IS_DIR: u32 = 3;
const COL_POPULATED: u32 = 4;

/// Row id reserved for the lazy-expand placeholder child. Never a key in
/// [`WorkspacePanel::paths`], so pruning it is a no-op.
const PLACEHOLDER_ID: u64 = 0;

/// Freedesktop icon-theme names for the two row kinds. Present in every
/// standard icon theme; a per-mime lookup is deferred polish.
const ICON_FOLDER: &str = "folder";
const ICON_FILE: &str = "text-x-generic";

/// The action bar's progress label at rest — empty; the panel title
/// lives on the dock group's caption. Shows "Expanding folders: N"
/// during an Unfold All walk.
const PROGRESS_IDLE: &str = "";

/// "Unfold All" async-walk tuning, mirroring Win32's batched walk.
/// Folders are expanded [`UNFOLD_BATCH`] per [`UNFOLD_TICK_MS`] timer
/// tick; the tree is *not* re-expanded row-by-row during the walk —
/// children are read into the model under collapsed rows, so no
/// per-folder repaint happens, and a single [`expand_populated`] pass
/// reveals what was read at the end. The action bar's progress label
/// shows an "Expanding folders: N" counter meanwhile. 20 folders / 15 ms ≈ 1300 folders/s,
/// the same envelope as the Win32 walk — and, like it, the GTK main loop
/// keeps pumping between ticks so the UI stays responsive on a huge tree.
const UNFOLD_BATCH: usize = 20;
const UNFOLD_TICK_MS: u64 = 15;

/// Ceiling on folders an Unfold All will *descend into* before it stops
/// itself and leaves the rest collapsed (still lazily expandable by
/// click). Bounds the directory reads a pathological tree (a cloned repo
/// with a huge `node_modules`, a fork bomb of empty dirs) turns one click
/// into. Paired with [`UNFOLD_MAX_ROWS`], which bounds the *rows* those
/// folders materialise — folder count bounds I/O, row count bounds memory,
/// and the walk stops at whichever it reaches first.
const UNFOLD_MAX_FOLDERS: usize = 25_000;

/// Ceiling on rows a single Unfold All will materialise. [`UNFOLD_MAX_FOLDERS`]
/// alone doesn't bound this — 25 000 folders each holding [`MAX_DIR_ENTRIES`]
/// files would be 125 M rows — so this caps how much *one click* can grow
/// the model, in the spirit of §8's memory floor. Whichever ceiling hits
/// first stops the walk. Note this bounds the walk's *own* insertions, not
/// the model's total row count: rows the user already lazy-expanded by hand
/// (or a previous walk left behind) aren't recounted — unbounded manual
/// accumulation over a session is a pre-existing property of lazy loading,
/// shared with Win32 and outside this cap's remit.
const UNFOLD_MAX_ROWS: usize = 100_000;

/// Per-tick ceiling on rows materialised, checked *between* folders. Keeps
/// one tick from synchronously inserting the whole of a fat batch (up to
/// [`UNFOLD_BATCH`] × [`MAX_DIR_ENTRIES`] rows) before yielding to the main
/// loop — the responsiveness half of the bound, distinct from the total
/// [`UNFOLD_MAX_ROWS`] memory half. A folder is populated atomically and the
/// check runs after it, so one tick can carry up to `UNFOLD_ROWS_PER_TICK - 1`
/// rows and then add a whole [`MAX_DIR_ENTRIES`]-sized directory on top —
/// a worst-case tick of roughly `UNFOLD_ROWS_PER_TICK + MAX_DIR_ENTRIES`
/// rows, still bounded and sub-millisecond.
const UNFOLD_ROWS_PER_TICK: usize = 2_000;

/// Per-directory entry cap. A single `read_dir` materialises at most this
/// many rows into the model; the surplus is replaced by one non-functional
/// "more items" marker row. Without it, one directory with millions of
/// entries would block the UI thread for the whole read regardless of the
/// inter-directory batching — the intra-directory analogue of
/// [`UNFOLD_MAX_FOLDERS`]. Applies to ordinary lazy expansion too, not
/// only Unfold All, so a giant directory never stalls the tree.
const MAX_DIR_ENTRIES: usize = 5_000;

thread_local! {
    /// The View-menu check item reflecting panel visibility, if built.
    static MENU_CHECK: std::cell::RefCell<Option<gtk::CheckMenuItem>> =
        const { std::cell::RefCell::new(None) };
    /// The toolbar toggle button reflecting panel visibility, if built.
    static TB_TOGGLE: std::cell::RefCell<Option<gtk::ToggleToolButton>> =
        const { std::cell::RefCell::new(None) };
    /// True while [`sync_indicators`] is programmatically setting the
    /// check/toggle state, so their own handlers don't loop back into
    /// [`set_visible`]. The workspace analogue of `menu::REFRESHING_MARKS`.
    static SYNCING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Everything the GTK backend owns for the workspace panel, held as a
/// field on `GtkUiState`.
pub struct WorkspacePanel {
    /// The panel content (action bar + tree) — the widget the dock
    /// hosts. Created once; never destroyed, only reparented by
    /// [`crate::dock`].
    container: gtk::Box,
    /// The directory tree.
    tree: gtk::TreeView,
    /// Backing model. See the module-level column constants.
    store: gtk::TreeStore,
    /// Real, unsanitized path for each row, keyed by its [`COL_ID`].
    paths: HashMap<u64, PathBuf>,
    /// Next id to hand out. Monotonic; ids are never reused within a
    /// session, so a stale [`gtk::TreeIter`] can't collide with a fresh row.
    next_id: u64,
    /// The workspace root, or `None` if none has been opened. Preserved
    /// across hide/show so re-toggling reopens the same folder.
    root: Option<PathBuf>,
    /// The action bar's "Expanding folders: N" progress counter during
    /// an Unfold All walk; empty at rest.
    progress: gtk::Label,
    /// DFS queue of folder rows still to expand in the async Unfold All
    /// walk; non-empty iff a walk is in progress. `GtkTreeStore`
    /// iterators are persistent, so these stay valid across the ticks'
    /// model inserts — only placeholder rows, never a queued folder, are
    /// ever removed.
    unfold_pending: Vec<gtk::TreeIter>,
    /// Folders expanded so far this walk — the counter shown in
    /// [`progress`].
    ///
    /// [`progress`]: Self::progress
    unfold_count: usize,
    /// Rows this walk has inserted, capped by [`UNFOLD_MAX_ROWS`]. Counts
    /// only the walk's own inserts, not rows already in the model — see the
    /// [`UNFOLD_MAX_ROWS`] note.
    unfold_rows: usize,
    /// True while an Unfold All walk is running. Guards re-entry (a second
    /// click is a no-op) and lets a still-scheduled tick self-cancel after
    /// [`cancel_unfold`] clears it.
    unfold_active: bool,
    /// Generation of the current walk, bumped on each [`unfold_all`]. Each
    /// scheduled tick captures the value it was started with and bails on
    /// mismatch, so a Fold-then-Expand within one tick interval can't leave
    /// the previous walk's still-scheduled `GSource` draining the new
    /// walk's queue ("exactly one live timer", made explicit rather than
    /// resting on the `unfold_active` flag alone).
    unfold_gen: u64,
}

impl WorkspacePanel {
    /// Build the panel content. The caller hands the returned
    /// [`Self::content`] to [`crate::dock::install`], which owns where
    /// it is shown from then on.
    pub fn build() -> Self {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);

        // Action bar: the progress counter on the left, then expand-all /
        // fold-all / locate right-aligned and packed tight (spacing 0, no
        // relief) so they don't hog width — Win32's action-button order,
        // `pack_end` in reverse so left→right still reads ⊞ ⊟ ◎. The
        // title and close ✕ that used to sit above this row are the dock
        // group's caption now.
        let action_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        action_row.set_margin_top(1);
        action_row.set_margin_bottom(1);
        action_row.set_margin_start(6);
        action_row.set_margin_end(2);
        let progress = gtk::Label::new(Some(PROGRESS_IDLE));
        progress.set_xalign(0.0);
        // A mid-walk ellipsis keeps "Expanding folders: N" from widening
        // the band.
        progress.set_ellipsize(gtk::pango::EllipsizeMode::End);
        action_row.pack_start(&progress, true, true, 0);
        let expand_btn = header_button("⊞", "Expand All");
        expand_btn.connect_clicked(|_| {
            crate::at_callback_boundary("workspace:expand_btn:clicked", (), unfold_all);
        });
        let fold_btn = header_button("⊟", "Fold All");
        fold_btn.connect_clicked(|_| {
            crate::at_callback_boundary("workspace:fold_btn:clicked", (), fold_all);
        });
        let locate_btn = header_button("◎", "Locate Current File");
        locate_btn.connect_clicked(|_| {
            crate::at_callback_boundary("workspace:locate_btn:clicked", (), locate_current);
        });
        action_row.pack_end(&locate_btn, false, false, 0);
        action_row.pack_end(&fold_btn, false, false, 0);
        action_row.pack_end(&expand_btn, false, false, 0);
        container.pack_start(&action_row, false, false, 0);

        let store = gtk::TreeStore::new(&[
            glib::Type::STRING, // icon name
            glib::Type::STRING, // display name (sanitized)
            glib::Type::U64,    // row id → paths map
            glib::Type::BOOL,   // is_dir
            glib::Type::BOOL,   // populated
        ]);
        let tree = gtk::TreeView::with_model(&store);
        tree.set_headers_visible(false);

        // One column carrying the icon and the name side by side.
        let column = gtk::TreeViewColumn::new();
        let icon = gtk::CellRendererPixbuf::new();
        gtk::prelude::TreeViewColumnExt::pack_start(&column, &icon, false);
        gtk::prelude::TreeViewColumnExt::add_attribute(
            &column,
            &icon,
            "icon-name",
            COL_ICON as i32,
        );
        let text = gtk::CellRendererText::new();
        gtk::prelude::TreeViewColumnExt::pack_start(&column, &text, true);
        gtk::prelude::TreeViewColumnExt::add_attribute(&column, &text, "text", COL_NAME as i32);
        tree.append_column(&column);

        let scroll = gtk::ScrolledWindow::builder().build();
        scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroll.add(&tree);
        container.pack_start(&scroll, true, true, 0);

        // Show the content's children once, then opt out of every later
        // `show_all` (the toplevel's at startup, a group's on build) so
        // visibility is the dock's to manage: hidden means parked, never
        // a `hide()` on this widget.
        container.show_all();
        container.set_no_show_all(true);

        connect_tree_signals(&tree);

        Self {
            container,
            tree,
            store,
            paths: HashMap::new(),
            next_id: PLACEHOLDER_ID + 1,
            root: None,
            progress,
            unfold_pending: Vec::new(),
            unfold_count: 0,
            unfold_rows: 0,
            unfold_active: false,
            unfold_gen: 0,
        }
    }

    /// The panel content, for [`crate::dock::install`].
    pub fn content(&self) -> &gtk::Widget {
        self.container.upcast_ref()
    }

    /// Hand out a fresh row id.
    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// Wire the tree's three input handlers. Split out of
/// [`WorkspacePanel::build`] for length.
fn connect_tree_signals(tree: &gtk::TreeView) {
    // Lazy populate on first expand.
    tree.connect_row_expanded(|_, iter, _| {
        crate::at_callback_boundary("workspace:tree:row_expanded", (), || {
            on_row_expanded(iter);
        });
    });
    // Double-click / Enter: open a file, toggle a folder.
    tree.connect_row_activated(|tree, path, _| {
        crate::at_callback_boundary("workspace:tree:row_activated", (), || {
            on_row_activated(tree, path);
        });
    });
    // Right-click: the per-kind context menu.
    tree.connect_button_press_event(|tree, ev| {
        crate::at_callback_boundary(
            "workspace:tree:button_press_event",
            glib::Propagation::Proceed,
            || on_button_press(tree, ev),
        )
    });
}

/// A small flat header button carrying a glyph and a tooltip. The theme's
/// default button padding is trimmed so single-glyph buttons sit close
/// together instead of each claiming a wide hit-target.
fn header_button(glyph: &str, tip: &str) -> gtk::Button {
    let button = gtk::Button::with_label(glyph);
    button.set_relief(gtk::ReliefStyle::None);
    WidgetExt::set_tooltip_text(&button, Some(tip));
    apply_compact_button_css(&button);
    button
}

/// CSS class the compact styling is scoped to. A class selector rather
/// than a blanket `button {}` so a future header button with an icon+label
/// child (itself a `button` node) isn't compressed by inheritance.
const COMPACT_BUTTON_CLASS: &str = "codepp-compact-header-btn";

/// Attach a shared `CssProvider` that shrinks a button's padding and drops
/// its theme minimum size, so the header's glyph buttons are tight. The
/// provider is built once and reused; failing to parse the (static) CSS is
/// cosmetic, so a parse error is logged and ignored rather than fatal.
/// Shared with the dock group captions' close ✕.
pub(crate) fn apply_compact_button_css(button: &gtk::Button) {
    thread_local! {
        static PROVIDER: gtk::CssProvider = {
            let provider = gtk::CssProvider::new();
            let css = format!(
                ".{COMPACT_BUTTON_CLASS} {{ padding: 1px 3px; min-width: 0; min-height: 0; }}"
            );
            if let Err(err) = provider.load_from_data(css.as_bytes()) {
                tracing::warn!(?err, "workspace: compact-button CSS failed to parse");
            }
            provider
        };
    }
    let context = button.style_context();
    context.add_class(COMPACT_BUTTON_CLASS);
    PROVIDER.with(|provider| {
        context.add_provider(provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    });
}

// --- Registration + indicator sync -----------------------------------

/// Register the View-menu check item so [`sync_indicators`] can keep it
/// in step with the panel's real visibility. Called by [`crate::menu`].
pub(crate) fn register_menu_check(item: gtk::CheckMenuItem) {
    MENU_CHECK.with(|c| *c.borrow_mut() = Some(item));
}

/// Register the toolbar toggle button. Called by [`crate::toolbar`].
pub(crate) fn register_toolbar_toggle(button: gtk::ToggleToolButton) {
    TB_TOGGLE.with(|t| *t.borrow_mut() = Some(button));
}

/// True while [`sync_indicators`] owns the check/toggle. The menu and
/// toolbar handlers bail on this so a programmatic `set_active` never
/// loops back into [`set_visible`].
pub(crate) fn syncing() -> bool {
    SYNCING.with(std::cell::Cell::get)
}

/// Drive both indicators to `visible` without re-firing their handlers.
/// Also called by [`crate::dock`] after every reconcile, since a drag
/// or a caption ✕ changes visibility without passing through here.
pub(crate) fn sync_indicators(visible: bool) {
    let _syncing = crate::FlagGuard::set(&SYNCING);
    MENU_CHECK.with(|c| {
        if let Some(item) = &*c.borrow() {
            item.set_active(visible);
        }
    });
    TB_TOGGLE.with(|t| {
        if let Some(button) = &*t.borrow() {
            button.set_active(visible);
        }
    });
}

// --- Public entry points ----------------------------------------------

/// File → "Open Folder as Workspace…": pick a folder, then show it.
pub(crate) fn open_folder_flow() {
    let Some(root) = choose_folder() else {
        return;
    };
    show_at(&root);
}

/// Show or hide the panel. The single funnel behind the View toggle, the
/// toolbar button and the header close button, so all three agree.
///
/// With no root yet, a request to show routes to the folder picker rather
/// than opening an empty panel — matching the Win32 toggle.
pub(crate) fn set_visible(visible: bool) {
    if visible {
        let root = with_state(|st| st.workspace.root.clone()).flatten();
        let model_empty =
            with_state(|st| st.workspace.store.iter_first().is_none()).unwrap_or(true);
        match resolve_show_action(root.as_deref(), model_empty) {
            ShowAction::Pick => {
                // No root yet: reflect "still hidden" on the indicators the
                // user just flipped, then run the picker; a chosen folder
                // re-shows.
                sync_indicators(false);
                open_folder_flow();
                return;
            }
            // Populate on first show. `apply_saved` deliberately seeds
            // `root` without populating a *hidden* restored panel — reading
            // the directory at cold start is work the §8 budget doesn't
            // want for a pane the user can't see — so the read is deferred
            // to here, the moment it becomes visible.
            ShowAction::Populate => {
                if let Some(root) = &root {
                    // `populate_root` clears the store; cancel any walk
                    // first so a queued tick can't touch invalidated iters.
                    // (Today a walk can't be active with an empty model, but
                    // this states the invariant rather than resting on it —
                    // the same defence-in-depth `within_root` uses.)
                    cancel_unfold();
                    with_state(|st| populate_root(&mut st.workspace, root));
                }
            }
            ShowAction::ShowOnly => {}
        }
    } else {
        // Hiding: stop any in-flight Unfold All so a queued tick can't
        // walk a pane the user just closed.
        cancel_unfold();
    }
    crate::dock::set_panel_visible(DockPanel::Workspace, visible);
    // The dock's reconcile syncs indicators and the session itself
    // whenever the model changed; these repeat that work (idempotently)
    // for the one case it does not run — hiding an already-hidden
    // panel — so the mirrors never go stale. Deliberate overlap.
    sync_indicators(visible);
    sync_to_shell();
}

/// What showing the panel should do, given whether a root is set and
/// whether the tree model is already populated. Pure so the branch that
/// caused the "blank pane after restart" regression stays unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum ShowAction {
    /// No root remembered — run the folder picker.
    Pick,
    /// Root known but the model is empty (a restored-but-hidden root, or
    /// a first-ever show) — read the directory before showing.
    Populate,
    /// Root known and already populated — just reveal the pane.
    ShowOnly,
}

fn resolve_show_action(root: Option<&Path>, model_empty: bool) -> ShowAction {
    match (root, model_empty) {
        (None, _) => ShowAction::Pick,
        (Some(_), true) => ShowAction::Populate,
        (Some(_), false) => ShowAction::ShowOnly,
    }
}

/// Open `root` as the workspace directly, with no folder picker — for
/// callers that already know the directory (e.g. File → Open Containing
/// Folder → Folder as Workspace). Idempotent; re-roots in place if a
/// workspace is already open.
pub(crate) fn open_at(root: &Path) {
    show_at(root);
}

/// Root the panel at `root`, populate it, and show it. Idempotent —
/// re-roots in place if a workspace is already open.
fn show_at(root: &Path) {
    // Re-rooting clears the store, invalidating any iters a running walk
    // has queued — stop it first.
    cancel_unfold();
    with_state(|st| {
        st.workspace.root = Some(root.to_path_buf());
        populate_root(&mut st.workspace, root);
    });
    crate::dock::set_panel_visible(DockPanel::Workspace, true);
    sync_indicators(true);
    sync_to_shell();
}

/// Cold-start half of the restore that is this panel's to do: seed the
/// root the last session left open (the dock model, restored by
/// [`crate::dock::apply_saved`], says whether and where the panel is
/// shown) and, iff the panel is coming up visible, read the root
/// directory so the first paint carries the tree.
///
/// When the panel was closed-but-rooted at save, this seeds `root` only
/// and leaves the model empty — [`set_visible`] populates it the first
/// time the user re-shows the panel, so a hidden restored workspace
/// costs no cold-start `read_dir`. Re-showing then finds content, not a
/// blank pane. The caller has already checked the root still exists.
pub(crate) fn restore_root(root: &Path, populate: bool) {
    with_state(|st| {
        st.workspace.root = Some(root.to_path_buf());
        if populate {
            populate_root(&mut st.workspace, root);
        }
    });
}

/// Snapshot the live panel state into the shell so the next
/// `save_session` persists it. Called from the autosave / shutdown path.
/// The dock layout itself persists separately (`dock::sync_to_shell`);
/// this legacy mirror carries the **root path** — content state, not
/// layout — plus visible/width for downgrade tolerance.
pub(crate) fn sync_to_shell() {
    with_state(|st| {
        let session = codepp_core::session::WorkspaceSession {
            root: st.workspace.root.clone(),
            visible: crate::dock::is_visible(DockPanel::Workspace),
            width: Some(crate::dock::legacy_band_width(DockPanel::Workspace)),
        };
        st.shell.set_workspace_session(Some(session));
    });
}

// --- Population --------------------------------------------------------

/// Clear the tree and rebuild it rooted at `root`, with the root row
/// expanded and its immediate children loaded.
fn populate_root(ws: &mut WorkspacePanel, root: &Path) {
    ws.store.clear();
    ws.paths.clear();
    ws.next_id = PLACEHOLDER_ID + 1;

    let name = root.file_name().map_or_else(
        || root.to_string_lossy().into_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let root_iter = insert_row(ws, None, &name, root.to_path_buf(), true);
    // The root is populated eagerly, then expanded so the user sees its
    // contents immediately — the Win32 `populate_workspace_root` shape.
    // `insert_row` seeded a lazy-expand placeholder; drop it first, or the
    // eager populate would leave an empty leading row under the root (the
    // lazy path clears it on first expand, but the root never takes that
    // path).
    clear_children(ws, &root_iter);
    populate_children(ws, &root_iter, root);
    set_populated(&ws.store, &root_iter, true);
    if let Some(path) = ws.store.path(&root_iter) {
        ws.tree.expand_row(&path, false);
    }
}

/// Read one directory and insert its entries under `parent`: folders
/// first, then files, each case-insensitively sorted — Explorer order.
/// A `read_dir` error is logged and treated as an empty directory.
///
/// Capped at [`MAX_DIR_ENTRIES`] rows; a directory with more gets one
/// trailing non-functional "more items" marker instead of the surplus, so
/// a single giant directory can neither stall the UI thread nor bloat the
/// model unboundedly (the intra-directory half of the Unfold All bound).
/// Symlinks are classified by their own `file_type` (Win32 / file-manager
/// parity), so a symlinked directory shows as a leaf and is not descended
/// — which also keeps symlink-loop recursion unreachable.
/// Returns the number of rows inserted (entries plus the truncation
/// marker, if any) so the Unfold All walk can budget against it.
fn populate_children(ws: &mut WorkspacePanel, parent: &gtk::TreeIter, dir: &Path) -> usize {
    let (entries, truncated) = read_dir_sorted(dir);
    let mut inserted = entries.len();
    for (name, path, is_dir) in entries {
        insert_row(ws, Some(parent), &name, path, is_dir);
    }
    if truncated {
        insert_truncation_marker(&ws.store, parent);
        inserted += 1;
    }
    inserted
}

/// Directory entries as `(display-source name, full path, is_dir)`, sorted
/// folders-first then case-insensitively by name, plus a `truncated` flag
/// set when the directory held more than [`MAX_DIR_ENTRIES`] entries.
///
/// At most `MAX_DIR_ENTRIES + 1` entries are read from the iterator (one
/// extra only to detect the overflow), so the expensive `file_type` / path
/// work is bounded regardless of how large the directory actually is.
fn read_dir_sorted(dir: &Path) -> (Vec<(String, PathBuf, bool)>, bool) {
    let reader = match std::fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(err) => {
            tracing::warn!(?err, ?dir, "workspace: read_dir failed");
            return (Vec::new(), false);
        }
    };
    let mut entries: Vec<(String, PathBuf, bool)> = reader
        .filter_map(Result::ok)
        // One past the cap: enough to know the directory overflowed
        // without walking (and stat-ing) all of a million-entry dir.
        .take(MAX_DIR_ENTRIES + 1)
        .map(|entry| {
            let path = entry.path();
            // `file_type()` avoids a stat where the OS already knows; fall
            // back to `is_dir` on the rare platforms that don't fill it in.
            let is_dir = entry
                .file_type()
                .map_or_else(|_| path.is_dir(), |ft| ft.is_dir());
            let name = entry.file_name().to_string_lossy().into_owned();
            (name, path, is_dir)
        })
        .collect();
    let truncated = entries.len() > MAX_DIR_ENTRIES;
    if truncated {
        entries.truncate(MAX_DIR_ENTRIES);
    }
    sort_dir_entries(&mut entries);
    (entries, truncated)
}

/// Insert the non-functional "directory truncated" marker row. It carries
/// [`PLACEHOLDER_ID`] (no side-map entry), so `row_path` returns `None`
/// and every click/context action safely no-ops on it — and, being a
/// non-folder, the Unfold All walk never enqueues it.
fn insert_truncation_marker(store: &gtk::TreeStore, parent: &gtk::TreeIter) {
    store.insert_with_values(
        Some(parent),
        None,
        &[
            (COL_ICON, &""),
            (
                COL_NAME,
                &format!("… more than {MAX_DIR_ENTRIES} items — not shown"),
            ),
            (COL_ID, &PLACEHOLDER_ID),
            (COL_IS_DIR, &false),
            (COL_POPULATED, &true),
        ],
    );
}

/// Order directory entries the way a file manager does: folders before
/// files, then case-insensitively by name. Split out as a pure function
/// over `(name, path, is_dir)` so the ordering contract is unit-testable
/// without touching the filesystem.
fn sort_dir_entries(entries: &mut [(String, PathBuf, bool)]) {
    entries.sort_by(|a, b| {
        b.2.cmp(&a.2) // is_dir true sorts first
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
}

/// Insert one row, sanitizing the display name and stashing the real
/// path in the side map. Folders get a placeholder child so the expander
/// shows and the real read is deferred to first expand.
fn insert_row(
    ws: &mut WorkspacePanel,
    parent: Option<&gtk::TreeIter>,
    real_name: &str,
    path: PathBuf,
    is_dir: bool,
) -> gtk::TreeIter {
    let id = ws.alloc_id();
    ws.paths.insert(id, path);
    let icon = if is_dir { ICON_FOLDER } else { ICON_FILE };
    let display = sanitize_filename_for_display(real_name);
    let iter = ws.store.insert_with_values(
        parent,
        None,
        &[
            (COL_ICON, &icon),
            (COL_NAME, &display),
            (COL_ID, &id),
            (COL_IS_DIR, &is_dir),
            (COL_POPULATED, &false),
        ],
    );
    if is_dir {
        insert_placeholder(&ws.store, &iter);
    }
    iter
}

/// Insert the empty placeholder child that gives a folder its expander.
fn insert_placeholder(store: &gtk::TreeStore, parent: &gtk::TreeIter) {
    store.insert_with_values(
        Some(parent),
        None,
        &[
            (COL_ICON, &""),
            (COL_NAME, &""),
            (COL_ID, &PLACEHOLDER_ID),
            (COL_IS_DIR, &false),
            (COL_POPULATED, &true),
        ],
    );
}

/// Remove every child of `parent`, pruning their ids from the side map.
/// Used to drop the placeholder before a real populate.
fn clear_children(ws: &mut WorkspacePanel, parent: &gtk::TreeIter) {
    while let Some(child) = ws.store.iter_children(Some(parent)) {
        let id = row_id(&ws.store, &child);
        ws.paths.remove(&id);
        ws.store.remove(&child);
    }
}

// --- Signal handlers --------------------------------------------------

/// Lazy-populate a folder the first time it expands.
fn on_row_expanded(iter: &gtk::TreeIter) {
    with_state(|st| ensure_populated(&mut st.workspace, iter));
}

/// Open a file (or toggle a folder) on double-click / Enter.
fn on_row_activated(tree: &gtk::TreeView, path: &gtk::TreePath) {
    let target = with_state(|st| {
        let ws = &st.workspace;
        let iter = ws.store.iter(path)?;
        let real = row_path(ws, &iter)?;
        let is_dir = row_bool(&ws.store, &iter, COL_IS_DIR);
        Some((real, is_dir))
    })
    .flatten();
    let Some((real, is_dir)) = target else {
        return;
    };
    if is_dir {
        // Let GTK's own expand/collapse handle the toggle.
        if tree.row_expanded(path) {
            tree.collapse_row(path);
        } else {
            tree.expand_row(path, false);
        }
        return;
    }
    if !within_root(&real) {
        return;
    }
    // Route through the shared open loop so a workspace open behaves
    // exactly like File → Open and drag-and-drop (dedupe, rebind, wake).
    crate::menu::open_paths(vec![real]);
}

/// Right-click: select the row under the pointer and show its menu.
fn on_button_press(tree: &gtk::TreeView, ev: &gtk::gdk::EventButton) -> glib::Propagation {
    if ev.button() != 3 {
        return glib::Propagation::Proceed;
    }
    let (x, y) = ev.position();
    let Some((Some(path), _, _, _)) = tree.path_at_pos(x as i32, y as i32) else {
        return glib::Propagation::Proceed;
    };
    tree.selection().select_path(&path);
    show_context_menu(&path);
    // Consume: our menu replaces GTK's default right-click behaviour.
    glib::Propagation::Stop
}

// --- Header actions ----------------------------------------------------

/// Collapse the whole tree (the root row stays, its subtree folds).
/// Cancels any in-flight Unfold All first, so a `collapse_all` can't race
/// the walk's expansions.
fn fold_all() {
    cancel_unfold();
    with_state(|st| st.workspace.tree.collapse_all());
}

/// Expand every folder in the tree, reading each unread directory once.
///
/// This mirrors the Win32 "Unfold All": an **async batched walk** rather
/// than a synchronous recursion, so a workspace with thousands of folders
/// stays responsive (DESIGN.md §8 — the UI thread must not block) and the
/// user sees progress. The action bar's progress label becomes an
/// "Expanding folders: N" counter, and — because the walk populates the model under *collapsed*
/// rows and only reveals ([`expand_populated`]) once at the end — there is
/// no per-folder repaint flicker, only a single paint when the tree is
/// finally revealed. A second click while a walk runs is a no-op.
///
/// The walk is bounded on every axis so one click can't exhaust the UI:
/// [`UNFOLD_MAX_FOLDERS`] directories descended, [`UNFOLD_MAX_ROWS`] rows
/// materialised total, [`MAX_DIR_ENTRIES`] rows per directory, and
/// [`UNFOLD_ROWS_PER_TICK`] rows per tick before yielding to the main
/// loop. The one thing it does *not* bound is wall-clock per tick — a
/// batch of slow/network `read_dir`s can still exceed one tick interval,
/// the same accepted risk the lazy single-expand path already carries.
fn unfold_all() {
    let generation = with_state(|st| {
        let ws = &mut st.workspace;
        if ws.unfold_active {
            return None; // already walking — ignore the extra click
        }
        let Some(root) = ws.store.iter_first() else {
            return None; // no workspace open
        };
        ws.unfold_pending.clear();
        ws.unfold_pending.push(root);
        ws.unfold_count = 0;
        ws.unfold_rows = 0;
        ws.unfold_active = true;
        ws.unfold_gen = ws.unfold_gen.wrapping_add(1);
        Some(ws.unfold_gen)
    })
    .flatten();
    let Some(generation) = generation else {
        return;
    };
    glib::timeout_add_local(
        std::time::Duration::from_millis(UNFOLD_TICK_MS),
        move || {
            crate::at_callback_boundary(
                "workspace:unfold_all:timeout",
                glib::ControlFlow::Break,
                || tick_unfold(generation),
            )
        },
    );
}

/// Whether the walk should stop: the queue drained, or a resource ceiling
/// (folders descended or rows materialised) was reached. Pure so the
/// ceilings that bound a pathological tree stay unit-testable.
fn unfold_should_stop(pending_empty: bool, folders: usize, rows: usize) -> bool {
    pending_empty || folders >= UNFOLD_MAX_FOLDERS || rows >= UNFOLD_MAX_ROWS
}

/// What one Unfold All tick decided, so the `with_state` borrow can be
/// dropped before touching the tree (`expand_row` fires `row-expanded`,
/// which re-enters `with_state`).
enum TickOutcome {
    /// More folders remain; the counter (updated in-place) keeps climbing.
    More,
    /// The queue drained — reveal the fully-expanded tree.
    Done,
    /// The walk was cancelled (Fold All / hide / remove) — just stop.
    Stop,
}

/// Process one batch of the Unfold All queue. Scheduled on a
/// [`UNFOLD_TICK_MS`] timeout; returns [`glib::ControlFlow::Break`] to
/// unschedule itself when the walk finishes or is cancelled.
///
/// `generation` is the walk this tick belongs to; a mismatch against the
/// panel's current generation means a newer walk superseded this one (a
/// Fold-then-Expand within one tick interval), so it bails without
/// touching the new walk's state.
fn tick_unfold(generation: u64) -> glib::ControlFlow {
    let outcome = with_state(|st| {
        let ws = &mut st.workspace;
        if !ws.unfold_active || ws.unfold_gen != generation {
            return TickOutcome::Stop; // cancelled or superseded since queued
        }
        // Rows inserted this tick — kept separate from the walk total so a
        // single fat batch yields to the main loop mid-tick rather than
        // inserting up to `UNFOLD_BATCH × MAX_DIR_ENTRIES` rows at once.
        let mut rows_this_tick = 0usize;
        for _ in 0..UNFOLD_BATCH {
            let Some(node) = ws.unfold_pending.pop() else {
                break;
            };
            // Reads this folder's directory on first touch (drops its
            // placeholder), a no-op if already populated. Does not expand
            // the view row — that is deferred to the final reveal
            // ([`expand_populated`]).
            let inserted = ensure_populated(ws, &node);
            ws.unfold_rows += inserted;
            rows_this_tick += inserted;
            // Enqueue its subfolder children (DFS via the stack).
            if let Some(child) = ws.store.iter_children(Some(&node)) {
                loop {
                    if row_bool(&ws.store, &child, COL_IS_DIR) {
                        // `TreeIter` is `Copy`; each push snapshots the
                        // iter's current position before `iter_next` moves it.
                        ws.unfold_pending.push(child);
                    }
                    if !ws.store.iter_next(&child) {
                        break;
                    }
                }
            }
            ws.unfold_count += 1;
            // Yield to the main loop once this tick has done enough work,
            // or once a resource ceiling is reached (checked here, not only
            // after the full batch, so a fat batch can't overshoot).
            if rows_this_tick >= UNFOLD_ROWS_PER_TICK
                || unfold_should_stop(false, ws.unfold_count, ws.unfold_rows)
            {
                break;
            }
        }
        if unfold_should_stop(
            ws.unfold_pending.is_empty(),
            ws.unfold_count,
            ws.unfold_rows,
        ) {
            if !ws.unfold_pending.is_empty() {
                // Hit a resource ceiling with folders still queued: stop
                // here and leave them collapsed (still expandable by click)
                // rather than letting one click read an unbounded tree.
                tracing::warn!(
                    folders = ws.unfold_count,
                    rows = ws.unfold_rows,
                    "workspace: Unfold All hit a resource ceiling; remaining folders left collapsed"
                );
                ws.unfold_pending.clear();
            }
            ws.unfold_active = false;
            ws.progress.set_text(PROGRESS_IDLE);
            TickOutcome::Done
        } else {
            ws.progress
                .set_text(&format!("Expanding folders: {}", ws.unfold_count));
            TickOutcome::More
        }
    })
    .unwrap_or(TickOutcome::Stop);

    match outcome {
        TickOutcome::More => glib::ControlFlow::Continue,
        TickOutcome::Stop => glib::ControlFlow::Break,
        TickOutcome::Done => {
            // Reveal only what the walk read. See [`expand_populated`] for
            // why `tree.expand_all()` cannot be used here.
            expand_populated();
            glib::ControlFlow::Break
        }
    }
}

/// Reveal exactly what the walk read: expand every folder whose directory
/// has actually been populated, and no others.
///
/// **`GtkTreeView::expand_all` is the obvious call here and it defeats the
/// whole point of the ceilings.** `expand_all` walks every row and fires
/// `row-expanded` for each — which is the lazy-load hook
/// ([`on_row_expanded`] → [`ensure_populated`]). A folder the ceiling left
/// unread still carries its placeholder child (so its expander arrow
/// shows), so `expand_all` would expand it, drop the placeholder, and
/// `read_dir` it — recursively, synchronously, down every remaining
/// branch, with no ceiling in sight. That is the exact pathological
/// `node_modules` case [`UNFOLD_MAX_FOLDERS`] / [`UNFOLD_MAX_ROWS`] exist
/// to bound, and worse than no ceiling because it would happen in one
/// unbatched call instead of many yielded ticks. (Measured on the Cocoa
/// port: with the folder ceiling lowered to 5, a recursive reveal
/// populated all 341 directories against the correct 20.)
///
/// Expanding only already-populated folders reads nothing new — an
/// [`ensure_populated`] on a populated row returns early — leaving the
/// unread ones collapsed and still lazily expandable by click, which is
/// what the ceilings' own documentation says should happen. Mirrors
/// `ui_cocoa::workspace::expansion_order`.
///
/// Paths are collected under one borrow and expanded after it drops, so
/// the `row-expanded` each `expand_row` fires re-enters [`on_row_expanded`]
/// cleanly rather than as a re-entrant skip. Parents precede their children
/// (pre-order), so a child's row is already visible when it expands.
fn expand_populated() {
    let Some((tree, paths)) = with_state(|st| {
        let ws = &st.workspace;
        let mut paths = Vec::new();
        if let Some(root) = ws.store.iter_first() {
            collect_populated_dirs(&ws.store, root, &mut paths);
        }
        (ws.tree.clone(), paths)
    }) else {
        return;
    };
    for path in &paths {
        tree.expand_row(path, false);
    }
}

/// Pre-order collect the [`gtk::TreePath`] of every populated directory row
/// at or under `iter` and its siblings. Emit-then-descend guarantees a
/// parent path precedes its children, which [`expand_populated`] relies on.
/// Only descends into populated dirs, so it never touches an unread folder.
fn collect_populated_dirs(
    store: &gtk::TreeStore,
    iter: gtk::TreeIter,
    out: &mut Vec<gtk::TreePath>,
) {
    loop {
        if row_bool(store, &iter, COL_IS_DIR) && row_bool(store, &iter, COL_POPULATED) {
            if let Some(path) = store.path(&iter) {
                out.push(path);
            }
            if let Some(child) = store.iter_children(Some(&iter)) {
                collect_populated_dirs(store, child, out);
            }
        }
        if !store.iter_next(&iter) {
            break;
        }
    }
}

/// Stop an in-flight Unfold All and clear the progress label. A pending
/// tick, if any, sees `unfold_active == false` and unschedules itself.
/// Safe to call when no walk is running.
fn cancel_unfold() {
    with_state(|st| {
        let ws = &mut st.workspace;
        if ws.unfold_active {
            ws.unfold_active = false;
            ws.unfold_pending.clear();
            ws.unfold_count = 0;
            ws.progress.set_text(PROGRESS_IDLE);
        }
    });
}

/// Select and scroll to the active file's row, expanding ancestors as
/// needed. A no-op if there is no saved file active or it lies outside
/// the workspace root.
fn locate_current() {
    let active = with_state(|st| {
        st.shell
            .active_tab
            .and_then(|i| st.shell.tabs.get(i))
            .and_then(|t| t.path.clone())
    })
    .flatten();
    let Some(target) = active else {
        return;
    };
    with_state(|st| {
        let ws = &mut st.workspace;
        let Some(root) = ws.root.clone() else {
            return;
        };
        // Linux paths are case-sensitive, so an exact strip is correct
        // (unlike Win32's case-insensitive walk).
        let Ok(rel) = target.strip_prefix(&root) else {
            return;
        };
        let Some(root_iter) = ws.store.iter_first() else {
            return;
        };
        let mut cur = root_iter;
        let mut acc = root;
        for component in rel.components() {
            ensure_populated(ws, &cur);
            acc = acc.join(component);
            let Some(child) = find_child_by_path(ws, &cur, &acc) else {
                return;
            };
            if let Some(cur_path) = ws.store.path(&cur) {
                ws.tree.expand_row(&cur_path, false);
            }
            cur = child;
        }
        if let Some(path) = ws.store.path(&cur) {
            ws.tree.selection().select_path(&path);
            ws.tree
                .scroll_to_cell(Some(&path), None::<&gtk::TreeViewColumn>, false, 0.0, 0.0);
        }
    });
}

/// Populate `iter`'s children if it is an unpopulated folder — the one
/// place the placeholder is dropped and a directory is read. Shared by
/// the lazy `row-expanded` path ([`on_row_expanded`]) and
/// [`locate_current`]'s ancestor reveal, so the two can't drift.
///
/// The directory listing itself is not re-checked against the workspace
/// root; only the leaf actions (open / launch / find) are, via
/// [`within_root`]. A hostile filesystem could therefore surface a name
/// under an escaped path in the tree, but no action on it launches
/// without the root re-check — the same scope the Win32 tree accepts
/// (DESIGN.md §7.4).
///
/// Returns the number of rows inserted — zero if the folder was already
/// populated (an idempotent no-op) — so the walk can budget against it.
fn ensure_populated(ws: &mut WorkspacePanel, iter: &gtk::TreeIter) -> usize {
    if row_bool(&ws.store, iter, COL_POPULATED) {
        return 0;
    }
    let Some(path) = row_path(ws, iter) else {
        return 0;
    };
    clear_children(ws, iter); // drops the placeholder
    let inserted = populate_children(ws, iter, &path);
    set_populated(&ws.store, iter, true);
    inserted
}

/// The direct child of `parent` whose real path equals `target`, if any.
fn find_child_by_path(
    ws: &WorkspacePanel,
    parent: &gtk::TreeIter,
    target: &Path,
) -> Option<gtk::TreeIter> {
    let child = ws.store.iter_children(Some(parent))?;
    loop {
        if row_path(ws, &child).as_deref() == Some(target) {
            return Some(child);
        }
        if !ws.store.iter_next(&child) {
            return None;
        }
    }
}

// --- Context menu ------------------------------------------------------

/// Build and pop up the per-kind context menu for the row at `path`.
fn show_context_menu(path: &gtk::TreePath) {
    let Some((real, is_dir, is_root)) = with_state(|st| {
        let ws = &st.workspace;
        let iter = ws.store.iter(path)?;
        let real = row_path(ws, &iter)?;
        let is_dir = row_bool(&ws.store, &iter, COL_IS_DIR);
        let is_root = ws.root.as_deref() == Some(real.as_path());
        Some((real, is_dir, is_root))
    })
    .flatten() else {
        return;
    };
    if !within_root(&real) {
        return;
    }

    let menu = gtk::Menu::new();
    if is_dir {
        if is_root {
            add_action(&menu, "Remove from Workspace", move |_| remove_root());
            add_separator(&menu);
        }
        add_copy_path(&menu, &real);
        let dir = real.clone();
        add_action(&menu, "Find in Files…", move |_| {
            if within_root(&dir) {
                crate::search::show_find_in_files_at(&dir.to_string_lossy());
            }
        });
        add_show_in_file_manager(&menu, real.clone());
    } else {
        let open = real.clone();
        add_action(&menu, "Open", move |_| {
            if within_root(&open) {
                crate::menu::open_paths(vec![open.clone()]);
            }
        });
        add_copy_path(&menu, &real);
        if let Some(name) = real.file_name() {
            let name = name.to_string_lossy().into_owned();
            add_action(&menu, "Copy File Name", move |_| copy_to_clipboard(&name));
        }
        let run = real.clone();
        add_action(&menu, "Run by System", move |_| {
            if within_root(&run) {
                open_in_default_app(&run);
            }
        });
        // "Show in File Manager" opens the parent so the file is revealed
        // in context, matching the Win32 "Explorer here" for a file.
        if let Some(parent) = real.parent() {
            add_show_in_file_manager(&menu, parent.to_path_buf());
        }
    }
    menu.show_all();
    // `None` uses the current event (this button-press), popping up at the
    // pointer — the non-deprecated replacement for `popup_easy`.
    // `gtk_menu_popup_at_pointer` refs the menu itself for the lifetime of
    // its grab, so dropping this local when the function returns does not
    // finalise the visible menu.
    menu.popup_at_pointer(None);
}

/// Append "Copy Path", copying the row's real (unsanitized) path.
fn add_copy_path(menu: &gtk::Menu, path: &Path) {
    let text = path.to_string_lossy().into_owned();
    add_action(menu, "Copy Path", move |_| copy_to_clipboard(&text));
}

/// Append "Show in File Manager", launching the default handler for the
/// directory (the file manager, on a folder URI). Re-checks `within_root`
/// at click time, like every other launch/open action, so a root change
/// between menu build and click can't launch outside the workspace.
fn add_show_in_file_manager(menu: &gtk::Menu, dir: PathBuf) {
    add_action(menu, "Show in File Manager", move |_| {
        if within_root(&dir) {
            open_in_default_app(&dir);
        }
    });
}

/// Append a menu item bound to `action`.
fn add_action(menu: &gtk::Menu, label: &str, action: impl Fn(&gtk::MenuItem) + 'static) {
    let item = gtk::MenuItem::with_label(label);
    item.connect_activate(move |item| {
        crate::at_callback_boundary("workspace:context_item:activate", (), || action(item));
    });
    menu.append(&item);
}

/// Append a separator.
fn add_separator(menu: &gtk::Menu) {
    menu.append(&gtk::SeparatorMenuItem::new());
}

/// Remove the workspace root and hide the panel, keeping no root so the
/// next show routes to the picker.
fn remove_root() {
    // Clearing the store invalidates any iters a running walk queued.
    cancel_unfold();
    with_state(|st| {
        let ws = &mut st.workspace;
        ws.store.clear();
        ws.paths.clear();
        ws.root = None;
    });
    crate::dock::set_panel_visible(DockPanel::Workspace, false);
    sync_indicators(false);
    sync_to_shell();
}

// --- Small platform actions -------------------------------------------

/// Copy `text` to the system clipboard.
fn copy_to_clipboard(text: &str) {
    let clip = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
    clip.set_text(text);
}

/// Launch the default application for `path` via GIO — the Linux
/// analogue of Win32's `ShellExecuteW(open)`. Logs on failure rather than
/// surfacing a dialog: a missing handler is not a data-loss event.
fn open_in_default_app(path: &Path) {
    let uri = match glib::filename_to_uri(path, None) {
        Ok(uri) => uri,
        Err(err) => {
            tracing::warn!(?err, ?path, "workspace: filename_to_uri failed");
            return;
        }
    };
    if let Err(err) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        tracing::warn!(?err, ?uri, "workspace: launch_default_for_uri failed");
    }
}

/// Pick a workspace folder with a native chooser. `None` on cancel.
fn choose_folder() -> Option<PathBuf> {
    let window = with_state(|st| st.window.clone())?;
    let chooser = gtk::FileChooserNative::new(
        Some("Open Folder as Workspace"),
        Some(&window),
        gtk::FileChooserAction::SelectFolder,
        Some("_Open"),
        Some("_Cancel"),
    );
    // `FileChooserNative` blocks in `run` and keeps its window alive until
    // dropped, matching the Open / Save-As choosers in `menu`.
    let path = if chooser.run() == gtk::ResponseType::Accept {
        chooser.filename()
    } else {
        None
    };
    chooser.destroy();
    path
}

// --- Model helpers -----------------------------------------------------

/// Read the row's [`COL_ID`].
fn row_id(store: &gtk::TreeStore, iter: &gtk::TreeIter) -> u64 {
    store
        .value(iter, COL_ID as i32)
        .get()
        .unwrap_or(PLACEHOLDER_ID)
}

/// Read the row's real path from the side map.
fn row_path(ws: &WorkspacePanel, iter: &gtk::TreeIter) -> Option<PathBuf> {
    ws.paths.get(&row_id(&ws.store, iter)).cloned()
}

/// Read a boolean column off `iter`.
fn row_bool(store: &gtk::TreeStore, iter: &gtk::TreeIter, col: u32) -> bool {
    store.value(iter, col as i32).get().unwrap_or(false)
}

/// Set the row's populated flag.
fn set_populated(store: &gtk::TreeStore, iter: &gtk::TreeIter, populated: bool) {
    store.set_value(iter, COL_POPULATED, &populated.to_value());
}

/// Whether `path` is inside the current workspace root — the
/// defence-in-depth guard every functional action re-checks.
///
/// Syntactic only: `starts_with` validates the component chain, it does
/// not canonicalise, so a symlink *inside* the root whose target lies
/// outside still passes. Open / Run-by-System therefore follow such a
/// link — accepted, matching Explorer / Nautilus / VS Code and the Win32
/// tree, since this is a manual action on an item the user can see.
fn within_root(path: &Path) -> bool {
    with_state(|st| {
        st.workspace
            .root
            .as_deref()
            .is_some_and(|root| path.starts_with(root))
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{resolve_show_action, sort_dir_entries, ShowAction};
    use std::path::{Path, PathBuf};

    fn entry(name: &str, is_dir: bool) -> (String, PathBuf, bool) {
        (name.to_string(), PathBuf::from(name), is_dir)
    }

    #[test]
    fn folders_sort_before_files_then_case_insensitively() {
        let mut e = vec![
            entry("Zebra.txt", false),
            entry("apple", true),
            entry("Banana.md", false),
            entry("Cherry", true),
            entry("art.rs", false),
        ];
        sort_dir_entries(&mut e);
        let order: Vec<&str> = e.iter().map(|(n, _, _)| n.as_str()).collect();
        // Both folders first (apple, Cherry — case-insensitive), then the
        // three files (art.rs, Banana.md, Zebra.txt — case-insensitive).
        assert_eq!(
            order,
            vec!["apple", "Cherry", "art.rs", "Banana.md", "Zebra.txt"]
        );
    }

    #[test]
    fn unfold_stops_when_drained_or_at_a_ceiling() {
        use super::{unfold_should_stop, UNFOLD_MAX_FOLDERS, UNFOLD_MAX_ROWS};
        // Not empty, under both ceilings → keep going.
        assert!(!unfold_should_stop(false, 0, 0));
        assert!(!unfold_should_stop(
            false,
            UNFOLD_MAX_FOLDERS - 1,
            UNFOLD_MAX_ROWS - 1
        ));
        // Queue drained → stop, however little was processed.
        assert!(unfold_should_stop(true, 0, 0));
        // Folder ceiling reached with work queued → stop (bounds I/O).
        assert!(unfold_should_stop(false, UNFOLD_MAX_FOLDERS, 0));
        // Row ceiling reached with few folders → stop (bounds memory), so
        // a handful of enormous directories can't blow past the folder cap.
        assert!(unfold_should_stop(false, 1, UNFOLD_MAX_ROWS));
        assert!(unfold_should_stop(false, 1, UNFOLD_MAX_ROWS + 1));
    }

    #[test]
    fn show_action_covers_every_branch() {
        // No root → run the picker, regardless of the model.
        assert_eq!(resolve_show_action(None, true), ShowAction::Pick);
        assert_eq!(resolve_show_action(None, false), ShowAction::Pick);
        // Rooted but empty model (restored-but-hidden root, or first show)
        // → populate before showing. This is the branch whose absence
        // caused the blank-pane-after-restart regression.
        let root = Path::new("/tmp/ws");
        assert_eq!(resolve_show_action(Some(root), true), ShowAction::Populate);
        // Rooted and already populated → just reveal the pane.
        assert_eq!(resolve_show_action(Some(root), false), ShowAction::ShowOnly);
    }
}
