//! The "Folder as Workspace" side panel: a lazily-populated directory
//! tree docked to the left of the editor. Port of `ui_gtk::workspace`,
//! which is itself a port of `ui_win32`'s `SysTreeView32` panel.
//!
//! # The value-vs-label split (security)
//!
//! A row's **display** text goes through
//! [`codepp_shell::sanitize_filename_for_display`], so a filename
//! carrying a bidi override or a zero-width character cannot spoof its
//! extension in the panel. The **real** path an open or a context action
//! operates on is never parsed back out of that display text: it lives
//! beside it in the node, and the outline view addresses rows by a
//! synthetic id rather than by their label.
//!
//! That split is not decoration. DESIGN.md §7.4 records the Win32 tree
//! shipping without it: a file named `invoice\u{202E}fdp.exe` rendered as
//! `invoice-exe.pdf` (the override reverses what follows), and the context menu's "Run by system" then handed
//! the real `.exe` to `ShellExecuteW` — a fake PDF that executes. Every
//! functional path here is additionally re-checked with
//! [`within_root`] at *click* time, defence-in-depth against a hostile
//! filesystem handing back a component with an absolute-path prefix, and
//! against a root change between building a menu and clicking it.
//!
//! # Why the model is a thread-local
//!
//! `NSOutlineView` is data-source driven, and AppKit asks for row counts
//! and cell views *whenever it lays out* — synchronously inside
//! `reloadData`, inside `expandItem:`, and inside a `setFrame:` that
//! resizes the view. Several of those originate from code already holding
//! a `with_state` borrow (the chrome relayout, for one). A data source
//! that reached back through `with_state` would be declined re-entrantly
//! on exactly those paths and would answer *zero rows*, blanking the tree
//! the moment the panel was resized. The same reasoning — and the same
//! shape — as [`crate::fif`]'s result rows.
//!
//! # Lazy population
//!
//! A folder row reports itself expandable without its directory having
//! been read; the read happens once, on `outlineViewItemWillExpand:`, and
//! the node is then marked populated and never re-read. `read_dir` is
//! bounded to [`MAX_DIR_ENTRIES`] per directory, so one enormous folder
//! cannot stall the UI thread, and Unfold All walks in timer-driven
//! batches with ceilings on both folders and rows.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, sel, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezelStyle, NSButton, NSColor, NSControlTextEditingDelegate,
    NSCursor, NSEvent, NSFont, NSImage, NSImageView, NSLineBreakMode, NSMenu, NSMenuItem,
    NSOutlineView, NSOutlineViewDataSource, NSOutlineViewDelegate, NSPasteboard,
    NSPasteboardTypeString, NSRectFill, NSScrollView, NSTableColumn,
    NSTableViewSelectionHighlightStyle, NSTextField, NSView, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSIndexSet, NSNotification, NSNumber, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString, NSURL,
};

use codepp_shell::sanitize_filename_for_display;

use crate::menu::Actions;
use crate::state::with_state;

/// The panel's resting header title, restored after an Unfold All walk.
const PANEL_TITLE: &str = "Folder as Workspace";

/// Default panel width in points, and the floor a drag cannot cross —
/// the same figures the other two backends use.
const DEFAULT_WIDTH: f64 = 240.0;
const MIN_WIDTH: f64 = 120.0;
/// The editor never shrinks below this, however far the divider is
/// dragged. Same role as the Document Map's `EDITOR_MIN_WIDTH`.
const EDITOR_MIN_WIDTH: f64 = 200.0;
/// Ceiling on a width restored from `session.xml`. See
/// [`crate::docmap::apply_saved`] for why a persisted geometry is bounded
/// on the way in.
const MAX_RESTORED_WIDTH: f64 = 10_000.0;
/// Smallest divider movement worth acting on, in points.
const DRAG_EPSILON: f64 = 0.5;

/// The panel's title row.
const HEADER_HEIGHT: f64 = 24.0;
/// The action-button row beneath the title. A second row rather than
/// sharing the title's — see [`build_header`].
const TOOLBAR_ROW_HEIGHT: f64 = 22.0;
/// Both header rows together, which is what the tree body sits below.
const HEADER_TOTAL: f64 = HEADER_HEIGHT + TOOLBAR_ROW_HEIGHT;
/// Edge of one header action button.
const BUTTON_EDGE: f64 = 20.0;
/// The drag handle down the panel's right edge.
const DIVIDER_WIDTH: f64 = 5.0;
/// Row height in the tree.
const ROW_HEIGHT: f64 = 18.0;

/// Per-directory entry cap. One `read_dir` materialises at most this many
/// rows; the surplus becomes a single non-functional "more items" marker.
/// Without it, a directory with millions of entries would block the UI
/// thread for the whole read however well the *inter*-directory batching
/// behaved. Applies to ordinary lazy expansion too, not only Unfold All.
const MAX_DIR_ENTRIES: usize = 5_000;

/// Unfold All tuning.
///
/// **A time budget per tick, not a fixed item count.** A count has to
/// guess at cost, and the guess is wrong in both directions: it stalls
/// when a batch turns out expensive (twenty `read_dir`s on a cold cache)
/// and wastes wall-clock when it turns out cheap, because the interval
/// then dominates. A deadline yields on time whatever the work costs, so
/// no tick can exceed roughly one frame plus one item — which is the
/// property that actually matters, since a stall here freezes the editor
/// and every other tab, not just this panel.
const UNFOLD_TICK_BUDGET_MS: f64 = 8.0;
/// Gap between ticks. Short enough to keep a large tree moving, long
/// enough that the run loop gets its turn.
const UNFOLD_TICK_SECS: f64 = 0.004;
/// Ceiling on folders one walk will descend into. Bounds the directory
/// reads a pathological tree (a huge `node_modules`, a fan-out of empty
/// directories) turns a single click into.
const UNFOLD_MAX_FOLDERS: usize = 25_000;
/// Ceiling on rows one walk will materialise. `UNFOLD_MAX_FOLDERS` alone
/// does not bound this — 25 000 folders each holding `MAX_DIR_ENTRIES`
/// files would be 125 M rows — so this caps how much one click can grow
/// the model, in the spirit of §8's memory floor. Whichever ceiling is
/// reached first stops the walk.
const UNFOLD_MAX_ROWS: usize = 100_000;

/// Id reserved for nothing: a real node never has it, so a stale or
/// synthetic item resolves to "gone" rather than to row zero.
const NO_ID: u64 = 0;

/// One row of the tree.
///
/// The `path`/`label` pair is the value-vs-label split the module docs
/// describe: `path` is what actions use, `label` is what the user sees,
/// and nothing ever derives one from the other.
struct Node {
    /// The real, unsanitized path. Never rendered.
    path: PathBuf,
    /// Display text, already sanitized. Never used to address anything.
    label: String,
    is_dir: bool,
    /// Whether this folder's directory has been read.
    populated: bool,
    children: Vec<u64>,
    /// A non-functional "directory truncated" row. Carries no usable
    /// path, so every click and context action no-ops on it.
    marker: bool,
}

thread_local! {
    /// The tree model. See the module docs for why this is not on
    /// `CocoaUiState`.
    static NODES: RefCell<HashMap<u64, Node>> = RefCell::new(HashMap::new());
    /// The root row's id, if a workspace is open.
    static ROOT_ID: Cell<u64> = const { Cell::new(NO_ID) };
    /// Next id to hand out. Monotonic and never reused within a session,
    /// so a stale item cannot collide with a fresh row.
    static NEXT_ID: Cell<u64> = const { Cell::new(NO_ID + 1) };
    /// The workspace root path, preserved across hide/show so re-toggling
    /// reopens the same folder.
    static ROOT_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static VISIBLE: Cell<bool> = const { Cell::new(false) };
    static WIDTH: Cell<f64> = const { Cell::new(DEFAULT_WIDTH) };
    /// Folder ids still to read in the in-flight Unfold All walk.
    static UNFOLD_QUEUE: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    /// Folders read and rows inserted so far this walk, against the two
    /// ceilings above.
    static UNFOLD_FOLDERS: Cell<usize> = const { Cell::new(0) };
    static UNFOLD_ROWS: Cell<usize> = const { Cell::new(0) };
    /// Generation of the current walk, bumped on every start. A scheduled
    /// tick captures the value it began with and bails on mismatch, so a
    /// Fold-then-Unfold inside one tick interval cannot leave the previous
    /// walk's timer draining the new walk's queue.
    static UNFOLD_GEN: Cell<u64> = const { Cell::new(0) };
    /// Folder ids still to *reveal*, once the read phase has finished.
    ///
    /// Held **reversed**, so `pop()` — which takes from the tail —
    /// hands them back in the parent-first order `expansion_order`
    /// produced. Seeded once by `begin_reveal` and only ever shrinks, so
    /// that order survives being split across ticks.
    static REVEAL_QUEUE: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

// --- Model helpers ----------------------------------------------------

fn alloc_id() -> u64 {
    NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    })
}

/// Read one field out of a node, or `None` if the id is stale.
fn with_node<R>(id: u64, f: impl FnOnce(&Node) -> R) -> Option<R> {
    NODES.with(|n| n.borrow().get(&id).map(f))
}

/// The real path for a row, or `None` for a stale id or a marker row.
fn node_path(id: u64) -> Option<PathBuf> {
    with_node(id, |n| (!n.marker).then(|| n.path.clone())).flatten()
}

/// Is `path` inside the current workspace root?
///
/// Re-checked at click time by every action that opens or launches, not
/// only when a menu is built — a root change in between must not let an
/// action escape the workspace. `false` when there is no root, which is
/// the safe direction: it refuses rather than allows.
fn within_root(path: &Path) -> bool {
    ROOT_PATH.with(|r| {
        r.borrow()
            .as_deref()
            .is_some_and(|root| path.starts_with(root))
    })
}

// --- The outline view -------------------------------------------------

/// Wrap a node id as the `id`-typed item `NSOutlineView` deals in.
fn item_for(id: u64) -> Retained<NSNumber> {
    NSNumber::new_u64(id)
}

/// Recover a node id from an outline item. `NO_ID` for the implicit root
/// item (`nil`), which is what AppKit passes for the top level.
fn id_of(item: Option<&NSObject>) -> u64 {
    item.and_then(|i| i.downcast_ref::<NSNumber>())
        .map_or(NO_ID, NSNumber::as_u64)
}

define_class!(
    // SAFETY: an `NSObject` subclass implementing two AppKit protocols
    // and adding no ivars. Main-thread-only: every callback arrives there.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppWorkspaceSource"]
    pub struct TreeSource;

    unsafe impl NSObjectProtocol for TreeSource {}

    unsafe impl NSOutlineViewDataSource for TreeSource {
        #[unsafe(method(outlineView:numberOfChildrenOfItem:))]
        fn number_of_children(&self, _view: &NSOutlineView, item: Option<&NSObject>) -> isize {
            // Zero is the safe fallback: an empty level, never a count
            // AppKit would then ask for rows it cannot get.
            crate::at_callback_boundary("workspace:numberOfChildren", 0, || {
                let count = match id_of(item) {
                    // The implicit root level: one row, the workspace folder.
                    NO_ID => usize::from(ROOT_ID.with(Cell::get) != NO_ID),
                    id => with_node(id, |n| n.children.len()).unwrap_or(0),
                };
                isize::try_from(count).unwrap_or(isize::MAX)
            })
        }

        #[unsafe(method_id(outlineView:child:ofItem:))]
        fn child_of_item(
            &self,
            _view: &NSOutlineView,
            index: isize,
            item: Option<&NSObject>,
        ) -> Retained<NSObject> {
            crate::at_callback_boundary(
                "workspace:childOfItem",
                item_for(NO_ID).into_super().into_super(),
                || {
                    let id = match id_of(item) {
                        NO_ID => ROOT_ID.with(Cell::get),
                        parent => usize::try_from(index)
                            .ok()
                            .and_then(|i| {
                                with_node(parent, |n| n.children.get(i).copied()).flatten()
                            })
                            .unwrap_or(NO_ID),
                    };
                    item_for(id).into_super().into_super()
                },
            )
        }

        #[unsafe(method(outlineView:isItemExpandable:))]
        fn is_expandable(&self, _view: &NSOutlineView, item: Option<&NSObject>) -> bool {
            crate::at_callback_boundary("workspace:isItemExpandable", false, || {
                with_node(id_of(item), |n| n.is_dir && !n.marker).unwrap_or(false)
            })
        }
    }

    unsafe impl NSControlTextEditingDelegate for TreeSource {}

    unsafe impl NSOutlineViewDelegate for TreeSource {
        /// Build the row: a small icon plus the sanitized label.
        #[unsafe(method_id(outlineView:viewForTableColumn:item:))]
        fn view_for_item(
            &self,
            view: &NSOutlineView,
            _column: Option<&NSTableColumn>,
            item: Option<&NSObject>,
        ) -> Option<Retained<NSView>> {
            crate::at_callback_boundary("workspace:viewForItem", None, || {
                let mtm = MainThreadMarker::new()?;
                let (label, is_dir, marker) =
                    with_node(id_of(item), |n| (n.label.clone(), n.is_dir, n.marker))?;
                let _ = view;
                Some(row_view(&label, is_dir, marker, mtm))
            })
        }

        /// Read the folder's directory the first time it is opened.
        #[unsafe(method(outlineViewItemWillExpand:))]
        fn item_will_expand(&self, notification: &NSNotification) {
            crate::at_callback_boundary("workspace:itemWillExpand", (), || {
                let Some(info) = notification.userInfo() else {
                    return;
                };
                let Some(item) = info.objectForKey(&NSString::from_str("NSObject")) else {
                    return;
                };
                populate_if_needed(id_of(item.downcast_ref::<NSObject>()));
            });
        }
    }
);

impl TreeSource {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `init` on a freshly allocated instance of our own class,
        // which adds no ivars needing other initialisation.
        unsafe { msg_send![this, init] }
    }
}

/// One row's view: a 16 pt icon and the sanitized label.
fn row_view(label: &str, is_dir: bool, marker: bool, mtm: MainThreadMarker) -> Retained<NSView> {
    let container = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(200.0, ROW_HEIGHT)),
    );
    let text_x = if marker { 2.0 } else { 20.0 };
    if !marker {
        let image = NSImage::imageNamed(&NSString::from_str(if is_dir {
            "NSFolder"
        } else {
            "NSMultipleDocuments"
        }));
        if let Some(image) = image {
            let icon = NSImageView::initWithFrame(
                NSImageView::alloc(mtm),
                NSRect::new(NSPoint::new(1.0, 1.0), NSSize::new(16.0, 16.0)),
            );
            icon.setImage(Some(&image));
            container.addSubview(&icon);
        }
    }
    let text = NSTextField::labelWithString(&NSString::from_str(label), mtm);
    text.setFrame(NSRect::new(
        NSPoint::new(text_x, 0.0),
        NSSize::new(400.0, ROW_HEIGHT),
    ));
    text.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    text.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    if marker {
        text.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }
    text.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    container.addSubview(&text);
    container
}

// An `NSOutlineView` that routes right-clicks to our own menu.
define_class!(
    // SAFETY: an `NSOutlineView` subclass overriding one method, which it
    // does not chain (the whole point is to replace the default menu).
    #[unsafe(super(NSOutlineView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppWorkspaceTree"]
    pub struct Tree;

    unsafe impl NSObjectProtocol for Tree {}

    impl Tree {
        /// Show our own context menu for the row under the pointer.
        ///
        /// **`rightMouseDown:` rather than `menuForEvent:`**, which is the
        /// more obvious hook: the latter *returns* the menu, and an
        /// override with an object return type has to declare its
        /// retain semantics to objc2, whereas AppKit's own contract for a
        /// popped-up menu is plain. `popUpContextMenu:` is the documented
        /// way to present one by hand and needs no return value at all.
        ///
        /// Deliberately does **not** chain to `super`: replacing the
        /// default menu is the whole point, and chaining would let AppKit
        /// present its own afterwards.
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            crate::at_callback_boundary("workspace:rightMouseDown", (), || {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                let row = self.rowAtPoint(point);
                let Ok(index) = usize::try_from(row) else {
                    return;
                };
                // Select what was actually clicked, so the menu and the
                // highlight cannot disagree about which row it acts on.
                self.selectRowIndexes_byExtendingSelection(
                    &NSIndexSet::indexSetWithIndex(index),
                    false,
                );
                let Some(item) = self.itemAtRow(row) else {
                    return;
                };
                let Some(menu) = context_menu(id_of(item.downcast_ref::<NSObject>())) else {
                    return;
                };
                // `popUpContextMenu:` spins a nested run loop, and GCD's
                // main-queue source is serviced there — so a worker wake
                // fires *during* the menu. Every other nested loop on this
                // backend takes the same guard; without it a drain could
                // move `active_tab`, or a future worker-driven re-root
                // could swap the tree under the open menu.
                let _freeze = crate::DrainFreeze::new();
                NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
            });
        }
    }
);

impl Tree {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `initWithFrame:` is the designated initialiser and this
        // subclass adds no ivars needing other initialisation.
        unsafe { msg_send![this, initWithFrame: frame] }
    }
}

// --- The divider ------------------------------------------------------

define_class!(
    // SAFETY: an `NSView` subclass overriding only drawing, cursor rects
    // and mouse tracking.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppWorkspaceDivider"]
    pub struct Divider;

    unsafe impl NSObjectProtocol for Divider {}

    impl Divider {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("workspace:dividerDrawRect", (), || {
                NSColor::separatorColor().setFill();
                NSRectFill(self.bounds());
            });
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            crate::at_callback_boundary("workspace:resetCursorRects", (), || {
                // Deprecated in favour of `columnResizeCursorInDirections:`,
                // which is macOS 15+; Code++ has no such deployment floor.
                // Same call the other two dividers on this backend make.
                #[allow(deprecated)]
                let cursor = NSCursor::resizeLeftRightCursor();
                self.addCursorRect_cursor(self.bounds(), &cursor);
            });
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            crate::at_callback_boundary("workspace:dividerMouseDown", (), || {});
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            crate::at_callback_boundary("workspace:dividerMouseDragged", (), || {
                drag_divider_to(event.locationInWindow().x);
            });
        }
    }
);

impl Divider {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: as above.
        unsafe { msg_send![this, initWithFrame: frame] }
    }
}

// --- The panel --------------------------------------------------------

/// Everything the workspace panel owns for the window's lifetime.
///
/// `Clone` is a refcount bump per field — handed out by
/// `CocoaUiState::split` on every drain, so it must stay cheap.
#[derive(Clone)]
pub struct WorkspacePanel {
    /// The panel's root. The caller positions it; see
    /// `CocoaUi::relayout_chrome`.
    pub container: Retained<NSView>,
    tree: Retained<Tree>,
    /// Header title, doubling as the "Expanding folders: N" counter
    /// during an Unfold All walk.
    title: Retained<NSTextField>,
    /// Held because `NSOutlineView`'s `dataSource` and `delegate` are both
    /// **weak** — nothing else keeps this alive, and a released data
    /// source is a dangling reference AppKit will message.
    #[allow(dead_code)]
    source: Retained<TreeSource>,
}

impl WorkspacePanel {
    /// Build the panel, sized to `height`. Starts hidden.
    pub fn new(height: f64, actions: &Actions, mtm: MainThreadMarker) -> Self {
        let width = DEFAULT_WIDTH;
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
        );
        // Keeps its width and its distance from the left edge as the
        // window grows; the editor beside it absorbs the slack.
        container.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewHeightSizable
                | NSAutoresizingMaskOptions::ViewMaxXMargin,
        );
        container.setHidden(true);

        // --- the drag handle, down the right edge -------------------
        let divider = Divider::new(
            NSRect::new(
                NSPoint::new(width - DIVIDER_WIDTH, 0.0),
                NSSize::new(DIVIDER_WIDTH, height),
            ),
            mtm,
        );
        divider.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewHeightSizable
                | NSAutoresizingMaskOptions::ViewMinXMargin,
        );
        container.addSubview(&divider);

        let title = build_header(&container, width, height, actions, mtm);

        // --- the tree, filling what is left -------------------------
        let body = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(
                (width - DIVIDER_WIDTH).max(0.0),
                (height - HEADER_TOTAL).max(0.0),
            ),
        );
        let tree = Tree::new(body, mtm);
        tree.setRowHeight(ROW_HEIGHT);
        tree.setHeaderView(None);
        tree.setIndentationPerLevel(13.0);
        tree.setSelectionHighlightStyle(NSTableViewSelectionHighlightStyle::Regular);
        tree.setAllowsMultipleSelection(false);
        let column = NSTableColumn::initWithIdentifier(
            NSTableColumn::alloc(mtm),
            &NSString::from_str("name"),
        );
        column.setWidth(400.0);
        tree.addTableColumn(&column);
        // SAFETY: the column was just added to this view.
        unsafe { tree.setOutlineTableColumn(Some(&column)) };

        let source = TreeSource::new(mtm);
        // SAFETY: both are weak references; `source` is held by the
        // returned `WorkspacePanel`, which the window state owns.
        unsafe {
            tree.setDataSource(Some(objc2::runtime::ProtocolObject::from_ref(&*source)));
            tree.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*source)));
            tree.setTarget(Some(&**actions));
            tree.setDoubleAction(Some(sel!(codeppWorkspaceActivate:)));
        }

        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), body);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDocumentView(Some(&tree));
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        container.addSubview(&scroll);

        Self {
            container,
            tree,
            title,
            source,
        }
    }

    /// Repaint the tree from the model.
    fn reload(&self) {
        self.tree.reloadData();
    }

    /// Set the header text — the resting title, or the walk's counter.
    fn set_title(&self, text: &str) {
        self.title.setStringValue(&NSString::from_str(text));
    }
}

/// Build the header — a title row with the panel's own close button, and
/// a **second row beneath it** carrying the action buttons. Returns the
/// title, which doubles as the Unfold All progress counter.
///
/// **Two rows, not one.** The panel is narrow by design, so a title and
/// four buttons sharing a line leaves the title a few characters wide and
/// the buttons crowding it. This is the layout `ui_gtk` was corrected to
/// after the same report; the Cocoa port copied GTK's *original* single
/// row and inherited the problem.
///
/// `ViewMinYMargin` on every one of these is load-bearing: they have to
/// stay pinned to the panel's top edge as the window makes it taller. The
/// FIF dock's header shipped without it and buried itself under the
/// results table the first time anyone dragged it.
fn build_header(
    container: &NSView,
    width: f64,
    height: f64,
    actions: &Actions,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let title_y = height - HEADER_HEIGHT;
    let tools_y = height - HEADER_HEIGHT - TOOLBAR_ROW_HEIGHT;
    // The action row fills from the left; only the close button shares
    // the title row, where a window's close control belongs.
    let mut x = 4.0;
    let mut header_button = |glyph: &str, tip: &str, action, row_y: f64| {
        let button = NSButton::initWithFrame(
            NSButton::alloc(mtm),
            NSRect::new(
                NSPoint::new(x, row_y),
                NSSize::new(BUTTON_EDGE, TOOLBAR_ROW_HEIGHT),
            ),
        );
        button.setTitle(&NSString::from_str(glyph));
        button.setBezelStyle(NSBezelStyle::AccessoryBarAction);
        button.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        button.setToolTip(Some(&NSString::from_str(tip)));
        button.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        // SAFETY: the target is a weak reference to an object the window
        // state owns for the process lifetime, and the action is a
        // compile-time `sel!` literal `Actions` implements.
        unsafe {
            button.setTarget(Some(&**actions));
            button.setAction(Some(action));
        }
        container.addSubview(&button);
        x += BUTTON_EDGE + 2.0;
    };
    header_button("▸", "Fold All", sel!(codeppWorkspaceFoldAll:), tools_y);
    header_button("▾", "Unfold All", sel!(codeppWorkspaceUnfoldAll:), tools_y);
    header_button(
        "◎",
        "Locate Current File",
        sel!(codeppWorkspaceLocate:),
        tools_y,
    );

    // The close button keeps the title row, pinned to the trailing edge.
    let close = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect::new(
            NSPoint::new(width - DIVIDER_WIDTH - BUTTON_EDGE - 2.0, title_y),
            NSSize::new(BUTTON_EDGE, HEADER_HEIGHT),
        ),
    );
    close.setTitle(&NSString::from_str("✕"));
    close.setBezelStyle(NSBezelStyle::AccessoryBarAction);
    close.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    close.setToolTip(Some(&NSString::from_str("Close")));
    close.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    // SAFETY: as above.
    unsafe {
        close.setTarget(Some(&**actions));
        close.setAction(Some(sel!(codeppWorkspaceClose:)));
    }
    container.addSubview(&close);

    let title = NSTextField::labelWithString(&NSString::from_str(PANEL_TITLE), mtm);
    title.setFrame(NSRect::new(
        NSPoint::new(6.0, title_y),
        NSSize::new(
            (width - DIVIDER_WIDTH - BUTTON_EDGE - 12.0).max(0.0),
            HEADER_HEIGHT,
        ),
    ));
    title.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    title.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    container.addSubview(&title);
    title
}

/// The width the layout should give the panel: zero while closed,
/// otherwise clamped to what this window width can spare.
///
/// The clamp belongs on every layout pass, not only in the divider drag:
/// a persisted width outlives the window size that justified it, and a
/// plain resize is handled entirely by autoresizing masks, where the
/// editor is the only flexible view and absorbs the whole delta. See
/// `crate::docmap::width_for_layout`, which carries the same note for the
/// same reason.
pub(crate) fn width_for_layout(content_width: f64, docmap_width: f64) -> f64 {
    if !VISIBLE.with(Cell::get) {
        return 0.0;
    }
    clamp_panel_width(WIDTH.with(Cell::get), content_width, docmap_width)
}

/// Clamp a wanted panel width against what the window can spare once the
/// Document Map has taken its own column.
///
/// Pure so the boundaries — which is what a divider drag gets wrong —
/// are testable without a window server.
fn clamp_panel_width(wanted: f64, content_width: f64, docmap_width: f64) -> f64 {
    let ceiling = (content_width - docmap_width - EDITOR_MIN_WIDTH).max(0.0);
    if ceiling <= MIN_WIDTH {
        return ceiling;
    }
    wanted.clamp(MIN_WIDTH, ceiling)
}

/// Move the divider so the panel's right edge sits at `window_x`.
fn drag_divider_to(window_x: f64) {
    let Some((content_w, map_w)) = with_state(|st| {
        let (_, ui) = st.split();
        let content_w = ui
            .window
            .contentView()
            .map_or(0.0, |c| c.bounds().size.width);
        (content_w, crate::docmap::width_for_layout(content_w))
    }) else {
        return;
    };
    let width = clamp_panel_width(window_x, content_w, map_w);
    if (WIDTH.with(Cell::get) - width).abs() < DRAG_EPSILON {
        return;
    }
    WIDTH.with(|w| w.set(width));
    crate::relayout_chrome_bands();
    sync_to_shell();
}

// --- Public entry points ----------------------------------------------

/// Whether the panel is open. Read by the View menu's mark and the
/// toolbar toggle's repaint, so both are painted from one truth.
pub(crate) fn is_visible() -> bool {
    VISIBLE.with(Cell::get)
}

/// What showing the panel should do, given whether a root is set and
/// whether the model is already populated.
///
/// Pure, because the branch it encodes caused a "blank pane after
/// restart" regression on GTK and is worth keeping testable.
#[derive(Debug, PartialEq, Eq)]
enum ShowAction {
    /// No root remembered — run the folder picker.
    Pick,
    /// Root known but the model is empty (a restored-but-hidden root, or
    /// a first-ever show) — read the directory before showing.
    Populate,
    /// Root known and already populated — just reveal the panel.
    ShowOnly,
}

fn resolve_show_action(has_root: bool, model_empty: bool) -> ShowAction {
    match (has_root, model_empty) {
        (false, _) => ShowAction::Pick,
        (true, true) => ShowAction::Populate,
        (true, false) => ShowAction::ShowOnly,
    }
}

/// Show or hide the panel. The single funnel behind the View toggle, the
/// toolbar button and the header close button, so all three agree.
///
/// With no root yet, a request to show routes to the folder picker rather
/// than opening an empty panel — matching the other two backends.
pub(crate) fn set_visible(visible: bool) {
    if visible {
        let has_root = ROOT_PATH.with(|r| r.borrow().is_some());
        let empty = ROOT_ID.with(Cell::get) == NO_ID;
        match resolve_show_action(has_root, empty) {
            ShowAction::Pick => {
                // Reflect "still hidden" on the indicators the user just
                // flipped, then run the picker; a chosen folder re-shows.
                refresh_indicators();
                open_folder_flow();
                return;
            }
            ShowAction::Populate => {
                // Deferred to here rather than done at cold start:
                // reading a directory for a pane the user cannot see is
                // work §8's budget does not want.
                let root = ROOT_PATH.with(|r| r.borrow().clone());
                if let Some(root) = root {
                    cancel_unfold();
                    populate_root(&root);
                }
            }
            ShowAction::ShowOnly => {}
        }
    } else {
        // Stop any in-flight walk so a queued tick cannot keep reading
        // directories for a pane the user just closed.
        cancel_unfold();
    }
    VISIBLE.with(|v| v.set(visible));
    with_state(|st| {
        st.workspace.container.setHidden(!visible);
        st.workspace.reload();
        let (_, ui) = st.split();
        ui.relayout_chrome();
    });
    refresh_indicators();
    sync_to_shell();
}

/// Repaint the View-menu mark and the toolbar toggle from the model.
///
/// The toolbar's button is `PushOnPushOff`, so AppKit has already flipped
/// it when *it* is the source but not when the menu or the header close
/// button is. The menu resolves its own mark on open.
fn refresh_indicators() {
    with_state(|st| st.toolbar.refresh_workspace_toggle());
}

/// Run the folder picker, then root the panel at what it returns.
pub(crate) fn open_folder_flow() {
    let chosen = choose_folder();
    // **Flush after the picker, on *both* paths.** `DrainFreeze` defers
    // wakes rather than dropping them, and `Drop` only decrements the
    // depth — it does not flush. Without this an event that arrived while
    // the panel was open (a load completing, a watcher's reload prompt)
    // sits unpresented until some unrelated later wake happens to run.
    // Cancel matters as much as OK: nothing else on that path drains.
    crate::drain_shell();
    if let Some(root) = chosen {
        open_at(&root);
    }
}

/// Pick a workspace folder with an `NSOpenPanel` in folder mode.
///
/// The `DrainFreeze` is not optional: the panel spins a nested run loop
/// and GCD's main-queue source is serviced there, so a worker wake would
/// otherwise `drain` the shell — moving `active_tab` — while the user is
/// still choosing. Every modal on this backend takes the same guard.
fn choose_folder() -> Option<PathBuf> {
    let mtm = MainThreadMarker::new()?;
    let _freeze = crate::DrainFreeze::new();
    let panel = objc2_app_kit::NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(false);
    panel.setCanChooseDirectories(true);
    panel.setAllowsMultipleSelection(false);
    panel.setTitle(Some(&NSString::from_str("Open Folder as Workspace")));
    // `NSModalResponseOK` is 1.
    if panel.runModal() == 1 {
        panel.URL().and_then(|url| crate::url_to_path(&url))
    } else {
        None
    }
}

/// Root the panel at `root`, populate it and show it. Idempotent —
/// re-roots in place if a workspace is already open.
pub(crate) fn open_at(root: &Path) {
    // Re-rooting clears the model, so a queued walk tick must not still
    // be holding ids from the old tree.
    cancel_unfold();
    ROOT_PATH.with(|r| *r.borrow_mut() = Some(root.to_path_buf()));
    populate_root(root);
    VISIBLE.with(|v| v.set(true));
    with_state(|st| {
        st.workspace.container.setHidden(false);
        st.workspace.reload();
        // SAFETY: expanding an item this model owns.
        unsafe {
            st.workspace
                .tree
                .expandItem(Some(&item_for(ROOT_ID.with(Cell::get))));
        };
        let (_, ui) = st.split();
        ui.relayout_chrome();
    });
    refresh_indicators();
    sync_to_shell();
}

/// Cold-start restore. Seeds width and root, and shows the panel if it
/// was visible last time.
pub(crate) fn apply_saved() {
    let Some(Some(saved)) = with_state(|st| st.shell.saved_workspace_session()) else {
        return;
    };
    if let Some(width) = saved.width.and_then(restorable_width) {
        WIDTH.with(|w| w.set(width));
    }
    let Some(root) = saved.root else {
        return;
    };
    // Only reopen a root that still exists; a deleted folder leaves the
    // panel closed rather than showing an empty tree.
    if root.is_dir() {
        ROOT_PATH.with(|r| *r.borrow_mut() = Some(root.clone()));
        if saved.visible {
            open_at(&root);
        }
    }
}

/// A persisted width, if it is one this process could have written. See
/// [`crate::docmap`]'s equivalent for why a session geometry is bounded.
fn restorable_width(saved: i32) -> Option<f64> {
    let width = f64::from(saved);
    (MIN_WIDTH..=MAX_RESTORED_WIDTH)
        .contains(&width)
        .then_some(width)
}

/// Snapshot the live panel state into the shell so the next
/// `save_session` persists it.
pub(crate) fn sync_to_shell() {
    let root = ROOT_PATH.with(|r| r.borrow().clone());
    let visible = VISIBLE.with(Cell::get);
    let width = WIDTH.with(Cell::get);
    with_state(|st| {
        st.shell
            .set_workspace_session(Some(codepp_core::session::WorkspaceSession {
                root: root.clone(),
                visible,
                width: Some(width.round() as i32),
            }));
    });
}

// --- Population -------------------------------------------------------

/// Clear the model and rebuild it rooted at `root`, with the root row's
/// own children read eagerly so the panel is not empty on first show.
fn populate_root(root: &Path) {
    NODES.with(|n| n.borrow_mut().clear());
    // **`NEXT_ID` is deliberately not reset.** It stays monotonic for the
    // process, which is what the invariant on its declaration actually
    // requires: an id left behind on a context-menu item, or anywhere
    // else, must resolve to "gone" after a re-root rather than to
    // whatever row now happens to occupy that number. Resetting it made
    // every workspace's root row id 1, so a stale item would have
    // silently addressed the *new* tree. Not reachable today — nothing
    // worker-driven re-roots — but the guarantee is cheap and its absence
    // is invisible.

    let name = root.file_name().map_or_else(
        || root.to_string_lossy().into_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let id = insert_node(root.to_path_buf(), &name, true, false);
    ROOT_ID.with(|r| r.set(id));
    read_into(id);
}

/// Add one node and return its id. The label is sanitized here, once, so
/// no caller can accidentally render a raw filename.
fn insert_node(path: PathBuf, real_name: &str, is_dir: bool, marker: bool) -> u64 {
    let id = alloc_id();
    let label = if marker {
        real_name.to_string()
    } else {
        sanitize_filename_for_display(real_name)
    };
    NODES.with(|n| {
        n.borrow_mut().insert(
            id,
            Node {
                path,
                label,
                is_dir,
                populated: false,
                children: Vec::new(),
                marker,
            },
        );
    });
    id
}

/// Read `id`'s directory if it has not been read yet. Returns how many
/// rows it inserted, so the Unfold All walk can budget against it.
fn populate_if_needed(id: u64) -> usize {
    let needs = with_node(id, |n| n.is_dir && !n.populated && !n.marker).unwrap_or(false);
    if !needs {
        return 0;
    }
    read_into(id)
}

/// Read one directory into `parent`'s children: folders first, then
/// files, each case-insensitively sorted — Finder order.
fn read_into(parent: u64) -> usize {
    let Some(dir) = node_path(parent) else {
        return 0;
    };
    let (entries, truncated) = read_dir_sorted(&dir);
    let mut children = Vec::with_capacity(entries.len() + usize::from(truncated));
    let mut inserted = 0;
    for (name, path, is_dir) in entries {
        children.push(insert_node(path, &name, is_dir, false));
        inserted += 1;
    }
    if truncated {
        children.push(insert_node(
            dir.clone(),
            &format!("… more than {MAX_DIR_ENTRIES} items — not shown"),
            false,
            true,
        ));
        inserted += 1;
    }
    NODES.with(|n| {
        if let Some(node) = n.borrow_mut().get_mut(&parent) {
            node.children = children;
            node.populated = true;
        }
    });
    inserted
}

/// Directory entries as `(name, full path, is_dir)`, sorted folders-first
/// then case-insensitively, plus a `truncated` flag set when the
/// directory held more than [`MAX_DIR_ENTRIES`] entries.
///
/// At most `MAX_DIR_ENTRIES + 1` entries are read from the iterator (one
/// extra only to detect the overflow), so the `file_type` and path work
/// is bounded regardless of how large the directory actually is.
///
/// Symlinks are classified by their **own** `file_type`, so a symlinked
/// directory shows as a leaf and is never descended — which is Finder
/// parity, and also what makes symlink-loop recursion unreachable.
fn read_dir_sorted(dir: &Path) -> (Vec<(String, PathBuf, bool)>, bool) {
    let reader = match std::fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(err) => {
            tracing::warn!(?err, "workspace: read_dir failed");
            return (Vec::new(), false);
        }
    };
    let mut entries: Vec<(String, PathBuf, bool)> = reader
        .filter_map(Result::ok)
        .take(MAX_DIR_ENTRIES + 1)
        .map(|entry| {
            let path = entry.path();
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

/// Order entries the way a file manager does: folders before files, then
/// case-insensitively by name. Pure, so the ordering contract is testable
/// without touching the filesystem.
fn sort_dir_entries(entries: &mut [(String, PathBuf, bool)]) {
    entries.sort_by(|a, b| {
        b.2.cmp(&a.2) // is_dir true sorts first
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
}

// --- Row actions ------------------------------------------------------

/// A double-clicked row: open a file, toggle a folder.
pub(crate) fn activate_selected_row() {
    let Some((id, row)) = with_state(|st| {
        let row = st.workspace.tree.clickedRow();
        let item = (row >= 0).then(|| st.workspace.tree.itemAtRow(row))?;
        Some((id_of(item?.downcast_ref::<NSObject>()), row))
    })
    .flatten() else {
        return;
    };
    let Some((path, is_dir)) = with_node(id, |n| (n.path.clone(), n.is_dir)) else {
        return;
    };
    if with_node(id, |n| n.marker).unwrap_or(true) {
        return;
    }
    if is_dir {
        with_state(|st| {
            let item = item_for(id);
            // SAFETY: every one of these addresses an item this
            // model owns, on the main thread.
            unsafe {
                if st.workspace.tree.isItemExpanded(Some(&item)) {
                    st.workspace.tree.collapseItem(Some(&item));
                } else {
                    populate_if_needed(id);
                    st.workspace.tree.reloadData();
                    st.workspace.tree.expandItem(Some(&item));
                }
            }
        });
        let _ = row;
        return;
    }
    // Re-checked here rather than trusted from when the row was built.
    if !within_root(&path) {
        return;
    }
    crate::open_path(path);
}

/// Build the context menu for a row, or `None` for a stale/marker row.
fn context_menu(id: u64) -> Option<Retained<NSMenu>> {
    let mtm = MainThreadMarker::new()?;
    let (path, is_dir, marker) = with_node(id, |n| (n.path.clone(), n.is_dir, n.marker))?;
    if marker || !within_root(&path) {
        return None;
    }
    let is_root = ROOT_PATH.with(|r| r.borrow().as_deref() == Some(path.as_path()));

    let menu = NSMenu::new(mtm);
    let add = |label: &str, action| {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(label));
        item.setTag(isize::try_from(id).unwrap_or(0));
        // SAFETY: `Actions` implements every selector passed here, and
        // AppKit holds a menu item's target weakly against an object the
        // window state owns for the process lifetime.
        unsafe {
            item.setAction(Some(action));
        }
        menu.addItem(&item);
        item
    };
    if is_dir {
        if is_root {
            add("Remove from Workspace", sel!(codeppWorkspaceRemoveRoot:));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        add("Copy Path", sel!(codeppWorkspaceCopyPath:));
        add("Find in Files…", sel!(codeppWorkspaceFindInFiles:));
        add("Show in Finder", sel!(codeppWorkspaceRevealInFinder:));
    } else {
        add("Open", sel!(codeppWorkspaceOpen:));
        add("Copy Path", sel!(codeppWorkspaceCopyPath:));
        add("Copy File Name", sel!(codeppWorkspaceCopyName:));
        add(
            "Open with Default Application",
            sel!(codeppWorkspaceRunBySystem:),
        );
        add("Show in Finder", sel!(codeppWorkspaceRevealInFinder:));
    }
    // The menu's target is set on the whole menu rather than per item, so
    // one call covers every entry above.
    for i in 0..menu.numberOfItems() {
        if let Some(item) = menu.itemAtIndex(i) {
            with_state(|st| {
                // SAFETY: weak target on an object the state owns.
                unsafe { item.setTarget(Some(&**st.actions)) };
            });
        }
    }
    Some(menu)
}

/// Every context-menu command, dispatched by the sender's tag (a node
/// id). Each one re-checks [`within_root`] rather than trusting the id it
/// was built with — a root change between opening the menu and clicking
/// must not let an action escape the workspace.
pub(crate) fn context_command(command: Command, id: u64) {
    let Some(path) = node_path(id) else {
        return;
    };
    if !within_root(&path) {
        return;
    }
    match command {
        Command::Open => crate::open_path(path),
        Command::CopyPath => copy_to_clipboard(&path.to_string_lossy()),
        Command::CopyName => {
            if let Some(name) = path.file_name() {
                copy_to_clipboard(&name.to_string_lossy());
            }
        }
        Command::RunBySystem | Command::RevealInFinder => {
            // "Show in Finder" on a *file* reveals it in its parent, which
            // is what a file manager does; on a folder it opens the folder.
            let target = if matches!(command, Command::RevealInFinder) && path.is_file() {
                path.parent().map(Path::to_path_buf).unwrap_or(path)
            } else {
                path
            };
            if within_root(&target) {
                open_in_default_app(&target);
            }
        }
        Command::FindInFiles => {
            crate::search::show_find_in_files_at(&path.to_string_lossy());
        }
        Command::RemoveRoot => remove_root(),
    }
}

/// The context-menu commands, so `menu.rs` names them rather than
/// repeating selector-to-behaviour mapping.
#[derive(Clone, Copy)]
pub(crate) enum Command {
    Open,
    CopyPath,
    CopyName,
    RunBySystem,
    RevealInFinder,
    FindInFiles,
    RemoveRoot,
}

/// Drop the workspace root and hide the panel, keeping no root so the
/// next show routes to the picker.
fn remove_root() {
    cancel_unfold();
    NODES.with(|n| n.borrow_mut().clear());
    ROOT_ID.with(|r| r.set(NO_ID));
    ROOT_PATH.with(|r| *r.borrow_mut() = None);
    VISIBLE.with(|v| v.set(false));
    with_state(|st| {
        st.workspace.container.setHidden(true);
        st.workspace.reload();
        let (_, ui) = st.split();
        ui.relayout_chrome();
    });
    refresh_indicators();
    sync_to_shell();
}

fn copy_to_clipboard(text: &str) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    // SAFETY: writing a plain string for a standard pasteboard type.
    unsafe {
        pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
    }
}

/// Launch the default application for `path` — the macOS analogue of
/// Win32's `ShellExecuteW(open)` and GTK's `launch_default_for_uri`.
///
/// Every caller has already checked [`within_root`]; this is the one
/// place in the panel that hands a path to the system, so it is worth
/// stating that the check is the caller's and is not repeated here.
fn open_in_default_app(path: &Path) {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    NSWorkspace::sharedWorkspace().openURL(&url);
}

// --- Header actions ---------------------------------------------------

/// Collapse everything under the root. Cancels any in-flight walk first,
/// so a queued tick cannot race the collapse.
pub(crate) fn fold_all() {
    cancel_unfold();
    with_state(|st| {
        // SAFETY: `nil` collapses from the root down.
        unsafe { st.workspace.tree.collapseItem_collapseChildren(None, true) };
    });
}

/// Reveal the active buffer's file in the tree, expanding the folders on
/// the way down to it.
pub(crate) fn locate_current() {
    let Some(Some(path)) = with_state(|st| st.shell.active().and_then(|t| t.path.clone())) else {
        return;
    };
    let Some(root) = ROOT_PATH.with(|r| r.borrow().clone()) else {
        return;
    };
    let Ok(rel) = path.strip_prefix(&root) else {
        return;
    };
    // Walk down component by component, reading each directory as it is
    // reached — the file may be under folders the user never expanded.
    let mut id = ROOT_ID.with(Cell::get);
    let mut trail = vec![id];
    for component in rel.components() {
        populate_if_needed(id);
        let target = component.as_os_str();
        let next = with_node(id, |n| n.children.clone())
            .unwrap_or_default()
            .into_iter()
            .find(|child| node_path(*child).is_some_and(|p| p.file_name() == Some(target)));
        let Some(next) = next else {
            break;
        };
        id = next;
        trail.push(id);
    }
    with_state(|st| {
        st.workspace.reload();
        // Expand every ancestor, then select the row itself.
        for ancestor in trail.iter().take(trail.len().saturating_sub(1)) {
            // SAFETY: an ancestor id this walk just resolved.
            unsafe { st.workspace.tree.expandItem(Some(&item_for(*ancestor))) };
        }
        let row = unsafe { st.workspace.tree.rowForItem(Some(&item_for(id))) };
        if row >= 0 {
            if let Ok(row) = usize::try_from(row) {
                st.workspace.tree.selectRowIndexes_byExtendingSelection(
                    &NSIndexSet::indexSetWithIndex(row),
                    false,
                );
                st.workspace
                    .tree
                    .scrollRowToVisible(isize::try_from(row).unwrap_or(0));
            }
        }
    });
}

/// Expand every folder, reading each unread directory once.
///
/// **Two phases, both batched on the same timer**, because either one
/// done in a single pass freezes the app — and a freeze here blocks the
/// editor and every other tab, not just this panel.
///
/// [`tick_unfold`] reads directories into the model; when it drains or
/// hits a ceiling it hands off to [`begin_reveal`], and [`tick_reveal`]
/// then expands the folders it read. Each tick yields on a time budget,
/// and the reveal coalesces its expansions with
/// `beginUpdates`/`endUpdates` so no per-folder repaint happens. See
/// [`tick_reveal`] for the measurements behind both.
pub(crate) fn unfold_all() {
    let root = ROOT_ID.with(Cell::get);
    if root == NO_ID {
        return;
    }
    cancel_unfold();
    let generation = UNFOLD_GEN.with(|g| {
        let next = g.get().wrapping_add(1);
        g.set(next);
        next
    });
    UNFOLD_QUEUE.with(|q| *q.borrow_mut() = vec![root]);
    UNFOLD_FOLDERS.with(|c| c.set(0));
    UNFOLD_ROWS.with(|c| c.set(0));
    schedule_unfold_tick(generation);
}

/// Stop an in-flight walk and restore the header title.
fn cancel_unfold() {
    UNFOLD_QUEUE.with(|q| q.borrow_mut().clear());
    REVEAL_QUEUE.with(|q| q.borrow_mut().clear());
    // Bumping the generation is what actually stops an already-scheduled
    // tick: it captured the old value and bails on mismatch. Clearing the
    // queue alone would not, because the tick could refill it.
    UNFOLD_GEN.with(|g| g.set(g.get().wrapping_add(1)));
    with_state(|st| st.workspace.set_title(PANEL_TITLE));
}

/// Reveal exactly what the walk read: expand every folder whose
/// directory has actually been populated, and no others.
///
/// **`expandItem:expandChildren:YES` is the obvious call here and it
/// defeats the whole point of the ceilings.** A recursive expand walks
/// every reachable row and fires `outlineViewItemWillExpand:` for each
/// one — which is the lazy-load hook. So for a tree that stopped at
/// [`UNFOLD_MAX_FOLDERS`] or [`UNFOLD_MAX_ROWS`], the folders still
/// sitting unread would each get a fresh synchronous `read_dir`,
/// recursively, down every remaining branch, with no ceiling in sight.
/// The exact "huge `node_modules`" case the ceilings exist to bound
/// would still block the main thread — and worse than before, because it
/// would happen in one unbatched call instead of many yielded ticks.
///
/// Expanding only populated folders leaves the rest collapsed and still
/// lazily expandable by click, which is what the ceilings' own
/// documentation says should happen. Parents are expanded before their
/// children because an outline view cannot expand a row whose ancestor
/// is collapsed.
///
/// `ui_gtk`'s walk ends in an unconditional `expand_all()` with a comment
/// asserting every row is already populated — true when the queue
/// drained, false when a ceiling stopped it. Tracked in §7.4.
fn begin_reveal(generation: u64) {
    let order = expansion_order(ROOT_ID.with(Cell::get), |id| {
        with_node(id, |n| (n.is_dir && n.populated, n.children.clone()))
    });
    with_state(|st| st.workspace.reload());
    // Reversed, so a batch is one `split_off` from the tail and the
    // stepper walks it back into parent-first order.
    REVEAL_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        *q = order;
        q.reverse();
    });
    schedule_reveal_tick(generation);
}

/// One batch of the reveal.
///
/// **Both halves of this are load-bearing, and each was measured.**
/// Expanding 3 682 folders one at a time against the resulting 60 000-row
/// model took **18 seconds** of frozen main thread — every `expandItem:`
/// recomputes the view's row array, so the cost is quadratic in the tree.
/// Wrapping the run in `beginUpdates`/`endUpdates` coalesces that into
/// one recompute and brings it to **97 ms**, a 185× difference.
///
/// 97 ms would be tolerable on its own, but it scales with the tree: at
/// [`UNFOLD_MAX_FOLDERS`] the same rate is over half a second of dead UI.
/// So the reveal is *also* batched onto the same timer as the read, which
/// bounds any single tick however large the tree turns out to be — the
/// property the read phase already had, and the one that actually
/// matters, because a freeze here blocks the editor and every other tab
/// rather than just this panel. Reported by a user as exactly that.
fn tick_reveal(generation: u64) {
    if UNFOLD_GEN.with(Cell::get) != generation {
        return;
    }
    if REVEAL_QUEUE.with(|q| q.borrow().is_empty()) {
        with_state(|st| st.workspace.set_title(PANEL_TITLE));
        return;
    }
    with_state(|st| {
        let tree = &st.workspace.tree;
        let deadline = std::time::Instant::now();
        // SAFETY: balanced begin/end around expansions of ids this model
        // owns, on the main thread. Expanding an already-populated folder
        // cannot re-enter `read_dir`, which is why this walks a
        // precomputed order rather than calling `expandChildren:`.
        unsafe {
            tree.beginUpdates();
            loop {
                if deadline.elapsed().as_secs_f64() * 1000.0 >= UNFOLD_TICK_BUDGET_MS {
                    break;
                }
                let Some(id) = REVEAL_QUEUE.with(|q| q.borrow_mut().pop()) else {
                    break;
                };
                tree.expandItem(Some(&item_for(id)));
            }
            tree.endUpdates();
        }
    });
    let left = REVEAL_QUEUE.with(|q| q.borrow().len());
    with_state(|st| {
        st.workspace
            .set_title(&format!("Revealing folders: {left} left"));
    });
    schedule_reveal_tick(generation);
}

fn schedule_reveal_tick(generation: u64) {
    let when = dispatch2::DispatchTime::NOW.time((UNFOLD_TICK_SECS * 1e9) as i64);
    let _ = dispatch2::DispatchQueue::main().after(when, move || {
        crate::at_callback_boundary("workspace:revealTick", (), || tick_reveal(generation));
    });
}

/// The ids to expand, parents before children.
///
/// Split out of [`begin_reveal`] as a pure walk over a lookup so the
/// traversal has `cargo test` coverage — the toolkit half is one
/// `expandItem:` per returned id and has nothing left to get wrong.
///
/// `lookup` answers `(is an expandable populated folder, its children)`,
/// or `None` for an id the model no longer holds.
///
/// Three properties, all pinned by the tests below:
///
/// * **Parents come before their children**, which an outline view
///   requires — it cannot expand a row whose ancestor is collapsed. This
///   falls out of *emit-then-push* rather than out of the stack
///   discipline: a child is only ever pushed after its parent has been
///   popped and emitted, so LIFO versus FIFO is irrelevant here.
/// * **Each id is visited at most once**, because the model is a strict
///   tree — every id is allocated once and appended to exactly one
///   parent's `children`.
/// * **Descent stops at the first unpopulated folder**, and misses
///   nothing by doing so, because population is top-down: a folder is
///   only ever read after its parent has been read. That invariant lives
///   across `populate_root`, `tick_unfold` and `locate_current`, so it is
///   the one a future edit is most likely to break — which is exactly why
///   it is tested here rather than argued in a comment.
fn expansion_order(root: u64, lookup: impl Fn(u64) -> Option<(bool, Vec<u64>)>) -> Vec<u64> {
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let Some((expandable, children)) = lookup(id) else {
            continue;
        };
        if !expandable {
            continue;
        }
        order.push(id);
        stack.extend(children);
    }
    order
}

/// Should the walk stop? Pure, so the two ceilings and the drained case
/// stay testable without a filesystem.
fn unfold_should_stop(pending_empty: bool, folders: usize, rows: usize) -> bool {
    pending_empty || folders >= UNFOLD_MAX_FOLDERS || rows >= UNFOLD_MAX_ROWS
}

fn schedule_unfold_tick(generation: u64) {
    let when = dispatch2::DispatchTime::NOW.time((UNFOLD_TICK_SECS * 1e9) as i64);
    let _ = dispatch2::DispatchQueue::main().after(when, move || {
        crate::at_callback_boundary("workspace:unfoldTick", (), || tick_unfold(generation));
    });
}

/// One batch of the Unfold All walk.
fn tick_unfold(generation: u64) {
    // A stale timer from a cancelled or superseded walk.
    if UNFOLD_GEN.with(Cell::get) != generation {
        return;
    }
    let mut folders = UNFOLD_FOLDERS.with(Cell::get);
    let mut rows = UNFOLD_ROWS.with(Cell::get);
    let deadline = std::time::Instant::now();
    loop {
        if deadline.elapsed().as_secs_f64() * 1000.0 >= UNFOLD_TICK_BUDGET_MS {
            break;
        }
        let Some(id) = UNFOLD_QUEUE.with(|q| q.borrow_mut().pop()) else {
            break;
        };
        rows += populate_if_needed(id);
        folders += 1;
        // Enqueue this folder's own subfolders, skipping markers.
        let subs: Vec<u64> = with_node(id, |n| n.children.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|c| with_node(*c, |n| n.is_dir && !n.marker).unwrap_or(false))
            .collect();
        UNFOLD_QUEUE.with(|q| q.borrow_mut().extend(subs));
        if rows >= UNFOLD_MAX_ROWS {
            break;
        }
    }
    UNFOLD_FOLDERS.with(|c| c.set(folders));
    UNFOLD_ROWS.with(|c| c.set(rows));

    let drained = UNFOLD_QUEUE.with(|q| q.borrow().is_empty());
    if unfold_should_stop(drained, folders, rows) {
        UNFOLD_QUEUE.with(|q| q.borrow_mut().clear());
        begin_reveal(generation);
        return;
    }
    with_state(|st| {
        st.workspace
            .set_title(&format!("Expanding folders: {folders}"));
    });
    schedule_unfold_tick(generation);
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_panel_width, expansion_order, resolve_show_action, restorable_width,
        sort_dir_entries, unfold_should_stop, ShowAction, EDITOR_MIN_WIDTH, MAX_RESTORED_WIDTH,
        MIN_WIDTH, UNFOLD_MAX_FOLDERS, UNFOLD_MAX_ROWS,
    };
    use std::path::PathBuf;

    fn entry(name: &str, is_dir: bool) -> (String, PathBuf, bool) {
        (name.to_string(), PathBuf::from(name), is_dir)
    }

    #[test]
    fn folders_sort_before_files_then_case_insensitively() {
        let mut entries = vec![
            entry("zebra.txt", false),
            entry("Apple", true),
            entry("beta.rs", false),
            entry("alpha", true),
            entry("Beta.rs", false),
        ];
        sort_dir_entries(&mut entries);
        let names: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(names[0], "alpha");
        assert_eq!(names[1], "Apple");
        // Both directories come first, whatever their case.
        assert!(entries[0].2 && entries[1].2);
        assert!(!entries[2].2);
        assert_eq!(names[4], "zebra.txt");
    }

    #[test]
    fn show_action_covers_every_branch() {
        assert_eq!(resolve_show_action(false, true), ShowAction::Pick);
        assert_eq!(resolve_show_action(false, false), ShowAction::Pick);
        assert_eq!(resolve_show_action(true, true), ShowAction::Populate);
        assert_eq!(resolve_show_action(true, false), ShowAction::ShowOnly);
    }

    /// A fake tree: `id -> (is a populated folder, children)`.
    fn tree<'a>(
        rows: &'a [(u64, bool, &'a [u64])],
    ) -> impl Fn(u64) -> Option<(bool, Vec<u64>)> + 'a {
        move |id| {
            rows.iter()
                .find(|(i, _, _)| *i == id)
                .map(|(_, populated, kids)| (*populated, kids.to_vec()))
        }
    }

    /// Documents parent-before-child; **not** mutation-covered, and that
    /// is a property of the code rather than a gap in the test.
    ///
    /// Emitting on *pop* is what guarantees the ordering, so no plausible
    /// edit to this function breaks it: swapping the `order.push` and
    /// `stack.extend` lines produces an identical sequence (they do not
    /// interact within one iteration — tried, and the mutant passed), and
    /// switching the stack for a queue preserves it too. The ordering
    /// only breaks if a future edit emits children at *push* time, which
    /// is not a refactor anyone reaches for by accident.
    ///
    /// The invariant that genuinely can break is the one below.
    #[test]
    fn the_expansion_order_puts_parents_before_their_children() {
        // 1 ─ 2 ─ 4
        //   └ 3
        let order = expansion_order(
            1,
            tree(&[
                (1, true, &[2, 3]),
                (2, true, &[4]),
                (3, true, &[]),
                (4, true, &[]),
            ]),
        );
        let at = |id: u64| order.iter().position(|x| *x == id).expect("missing");
        assert!(
            at(1) < at(2) && at(1) < at(3),
            "root must precede its children"
        );
        assert!(
            at(2) < at(4),
            "a parent must precede its child at every depth"
        );
        assert_eq!(order.len(), 4, "every populated folder is expanded once");
    }

    #[test]
    fn the_expansion_stops_at_an_unread_folder_and_skips_files() {
        // 2 is a populated folder; 3 was never read; 5 is a file. Nothing
        // under 3 may be expanded — and by the top-down population
        // invariant, nothing under it *can* be populated either.
        let order = expansion_order(
            1,
            tree(&[
                (1, true, &[2, 3, 5]),
                (2, true, &[]),
                (3, false, &[4]),
                (4, true, &[]),
                (5, false, &[]),
            ]),
        );
        assert!(order.contains(&1) && order.contains(&2));
        assert!(
            !order.contains(&3) && !order.contains(&4),
            "descent must stop at an unread folder: expanding it would fire \
             outlineViewItemWillExpand: and re-enter the lazy loader, which is \
             what defeats the Unfold All ceilings"
        );
        assert!(!order.contains(&5), "a file is never expanded");
    }

    #[test]
    fn a_stale_or_empty_model_yields_nothing_rather_than_looping() {
        // The root id is not in the model at all — after a re-root, say.
        assert!(expansion_order(1, tree(&[])).is_empty());
        // A dangling child reference is skipped rather than panicking.
        let order = expansion_order(1, tree(&[(1, true, &[99])]));
        assert_eq!(order, vec![1]);
    }

    #[test]
    fn the_unfold_walk_stops_when_drained_or_at_a_ceiling() {
        assert!(unfold_should_stop(true, 0, 0), "a drained queue stops");
        assert!(
            !unfold_should_stop(false, 1, 1),
            "an ordinary tick continues"
        );
        assert!(
            unfold_should_stop(false, UNFOLD_MAX_FOLDERS, 0),
            "the folder ceiling stops the walk"
        );
        assert!(
            unfold_should_stop(false, 0, UNFOLD_MAX_ROWS),
            "the row ceiling stops the walk"
        );
    }

    #[test]
    fn the_editor_keeps_its_floor_beside_both_panels() {
        // Both side panels open on a 900 pt window: the workspace yields
        // so the editor keeps its floor.
        let map = 160.0;
        let ws = clamp_panel_width(10_000.0, 900.0, map);
        assert!(
            900.0 - map - ws >= EDITOR_MIN_WIDTH,
            "editor left with {} pt",
            900.0 - map - ws
        );
    }

    #[test]
    fn an_ordinary_width_passes_through_and_a_narrow_one_is_lifted() {
        assert!((clamp_panel_width(240.0, 1400.0, 0.0) - 240.0).abs() < f64::EPSILON);
        assert!((clamp_panel_width(10.0, 1400.0, 0.0) - MIN_WIDTH).abs() < f64::EPSILON);
        assert!((clamp_panel_width(-5.0, 1400.0, 0.0) - MIN_WIDTH).abs() < f64::EPSILON);
    }

    #[test]
    fn a_window_too_narrow_for_both_floors_gives_the_editor_priority() {
        // 260 pt of usable width against a 200 pt editor floor leaves 60,
        // under the panel's own 120 pt floor. The panel yields rather than
        // pushing the editor off-screen, and never goes negative.
        let width = clamp_panel_width(240.0, 260.0, 0.0);
        assert!((width - 60.0).abs() < f64::EPSILON, "got {width}");
        assert!(clamp_panel_width(240.0, 100.0, 0.0) >= 0.0);
        assert!(clamp_panel_width(240.0, 300.0, 280.0) >= 0.0);
    }

    #[test]
    fn a_hand_edited_session_width_is_refused() {
        assert_eq!(restorable_width(240), Some(240.0));
        assert_eq!(restorable_width(MIN_WIDTH as i32), Some(MIN_WIDTH));
        assert_eq!(
            restorable_width(MAX_RESTORED_WIDTH as i32),
            Some(MAX_RESTORED_WIDTH)
        );
        for hostile in [0, -1, i32::MIN, i32::MAX, MIN_WIDTH as i32 - 1] {
            assert_eq!(restorable_width(hostile), None, "accepted {hostile}");
        }
    }
}
