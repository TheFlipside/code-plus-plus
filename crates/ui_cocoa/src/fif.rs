//! Find-in-Files: the results dock, the event drain, and the jump path.
//!
//! The *input* UI is not here — it is the third tab of the Find/Replace
//! panel in [`crate::search`], sharing that panel's query field and
//! option checkboxes exactly as `ui_win32` and `ui_gtk` share theirs.
//! This module owns everything below that: the bottom results dock, the
//! drain that turns [`FifEvent`]s into rows, and double-click navigation
//! to a match.
//!
//! The search *engine* is entirely cross-platform (`codepp_core::fif` +
//! `codepp_shell::fif`). A backend only has to (1) build a [`FifRequest`]
//! from the dialog and call `Shell::start_fif`, (2) drain [`FifEvent`]s
//! on each §5.4 wake and render them, and (3) implement the jump.
//! Open-buffer Replace-in-Files needs no code here at all — `Shell::drain`
//! applies it through the shared `is_doc_dirty` / `replace_doc_text`
//! hooks this backend already provides.
//!
//! # Buffer-then-flush
//!
//! Match events are buffered as they arrive and flushed into the table
//! only on the terminal `Done`/`Cancelled` event, matching both other
//! backends. Live feedback while the job runs is the header's running
//! "Searching… N matches in M files"; one bulk reload at the end is
//! cheaper than per-file churn on a large result set.
//!
//! # Why the rows live in a `thread_local` and not on the window state
//!
//! This is the one structural difference from `ui_gtk`, and it is forced
//! by this backend's re-entrancy guard rather than chosen.
//!
//! `NSTableView` is data-source driven: AppKit asks for the row count and
//! for each visible row's view *whenever it lays out*, which includes
//! synchronously inside `reloadData` and inside a `setFrame:` that changes
//! the table's size. Several of those calls originate from code that is
//! already inside a `with_state` borrow — `finalize` flushes from the
//! drain, and `CocoaUi::relayout_chrome` resizes the dock from a
//! `UiPlatform` method. A data source that reached back through
//! `with_state` would be declined re-entrantly on exactly those paths and
//! would answer "zero rows", so the dock would blank itself the moment
//! results arrived or the toolbar was toggled.
//!
//! Keeping the rows in a main-thread `thread_local` that the data source
//! reads directly removes the borrow from the query path entirely. The
//! same reasoning `TabStrip`'s `SCROLL_OFFSET` is written against, for a
//! sharper reason: there the alternative was merely wrong, here it would
//! be intermittently wrong.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, sel, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezelStyle, NSBorderType, NSButton, NSColor,
    NSControlTextEditingDelegate, NSCursor, NSEvent, NSFont, NSLineBreakMode, NSScrollView,
    NSTableColumn, NSTableView, NSTableViewDataSource, NSTableViewDelegate, NSTextField, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use codepp_core::{FifMatch, FifQueryOpts, FifWalkOpts};
use codepp_scintilla_sys::{SCI_GOTOLINE, SCI_POSITIONFROMLINE, SCI_SETSEL};
use codepp_shell::{
    sanitize_path_for_display, sanitize_str_for_display, FifEvent, FifJobId, FifRequest, FifStats,
    OpenBufferOutcome, OpenFileOutcome,
};

use crate::menu::Actions;
use crate::state::with_state;

/// Height of the dock when it first opens, in points.
const DOCK_DEFAULT_HEIGHT: f64 = 180.0;
/// Floor for a user drag of the divider — below this the header and one
/// row no longer fit and the dock is not useful.
const DOCK_MIN_HEIGHT: f64 = 84.0;
/// The editor never shrinks below this, however far the divider is
/// dragged up.
const EDITOR_MIN_HEIGHT: f64 = 120.0;
/// The dock's own header row (summary label + Cancel + close).
const HEADER_HEIGHT: f64 = 24.0;
/// The drag handle along the dock's top edge.
const DIVIDER_HEIGHT: f64 = 5.0;
/// Row height in the results table.
const ROW_HEIGHT: f64 = 17.0;

const COL_FILE: &str = "file";
const COL_LINE: &str = "line";
const COL_MATCH: &str = "match";

thread_local! {
    /// The flushed result rows — see the module docs for why these are
    /// not on `CocoaUiState`.
    static ROWS: RefCell<Vec<ResultRow>> = const { RefCell::new(Vec::new()) };
    /// The dock's height in points. A `thread_local` rather than a field
    /// on [`FifDock`] because the dock is `Clone` (it is handed out by
    /// `CocoaUiState::split` on every drain), so a plain field would give
    /// each copy its own height and the divider drag would be forgotten
    /// on the next wake.
    static DOCK_HEIGHT: Cell<f64> = const { Cell::new(DOCK_DEFAULT_HEIGHT) };
}

/// One row of the results table: three display strings plus the
/// functional coordinates a double-click reads.
///
/// The split is deliberate and is the same one `ui_win32`'s
/// `fif_listview_index` and `ui_gtk`'s hidden `ListStore` columns make:
/// what is *shown* is sanitized for display, what is *used* to navigate
/// is the real path and byte span. Navigation must never parse the label
/// back, or a filename carrying bidi overrides would send the jump
/// somewhere other than where the row says.
struct ResultRow {
    file: String,
    line: String,
    text: String,
    jump: JumpTarget,
}

/// Where a double-click should land once the file is open.
#[derive(Clone)]
struct JumpTarget {
    path: PathBuf,
    line_no: u32,
    col_start: u32,
    col_end: u32,
}

/// One matched file, buffered until the job completes.
struct PendingFile {
    path: PathBuf,
    matches: Vec<FifMatch>,
    truncated: bool,
}

/// How a job ended, as observed on the terminal `FifEvent`.
enum Terminal {
    Done(FifStats),
    Cancelled,
}

/// Per-job runtime state. Lives on [`crate::state::CocoaUiState`] because
/// it is model-shaped rather than widget-shaped, and unlike the rows it
/// is only ever touched from inside a borrow we already hold.
#[derive(Default)]
pub struct FifJob {
    active: Option<FifJobId>,
    pending: Vec<PendingFile>,
    /// A double-click's target, applied once the file's load lands.
    pending_jump: Option<JumpTarget>,
    hit_files: usize,
    hit_matches: usize,
    /// Occurrences replaced in *open* buffers (via `Shell::drain`), which
    /// the worker's disk stats do not count.
    replaced_in_buffers: usize,
    /// Open files skipped because they had unsaved edits.
    skipped_dirty: usize,
    is_replace: bool,
}

impl FifJob {
    /// Reset for a freshly started job.
    fn begin(&mut self, job: FifJobId, is_replace: bool) {
        *self = Self {
            active: Some(job),
            is_replace,
            ..Self::default()
        };
    }

    /// The completion summary shown in the dock header.
    fn summary(&self, term: &Terminal) -> String {
        use std::fmt::Write as _;
        let stats = match term {
            Terminal::Cancelled => {
                let mut s = format!(
                    "Cancelled — {} matches in {} files so far",
                    self.hit_matches, self.hit_files
                );
                if self.is_replace && self.replaced_in_buffers > 0 {
                    let _ = write!(s, ", {} replaced in open buffers", self.replaced_in_buffers);
                }
                return s;
            }
            Terminal::Done(stats) => stats,
        };

        let mut s = if self.is_replace {
            let total = stats.total_replacements + self.replaced_in_buffers;
            let mut t = format!(
                "Replaced {total} occurrences in {} files on disk",
                stats.files_modified
            );
            if self.replaced_in_buffers > 0 {
                let _ = write!(t, ", {} in open buffers", self.replaced_in_buffers);
            }
            if self.skipped_dirty > 0 {
                let _ = write!(
                    t,
                    " (skipped {} open file(s) with unsaved changes)",
                    self.skipped_dirty
                );
            }
            t
        } else {
            format!(
                "{} matches in {} files ({} scanned)",
                stats.total_matches, stats.files_with_matches, stats.files_scanned
            )
        };
        if stats.global_cap_hit || stats.depth_cap_hit {
            s.push_str(" — results incomplete, some content was not searched");
        }
        s
    }
}

define_class!(
    // SAFETY: plain `NSObject` subclass with no ivars. Every method is
    // invoked by AppKit on the main thread, which is where the
    // `thread_local` row store it reads lives.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppFifTableSource"]
    pub struct TableSource;

    unsafe impl NSObjectProtocol for TableSource {}

    unsafe impl NSTableViewDataSource for TableSource {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> isize {
            // Zero is the safe fallback: an empty table, never a
            // count AppKit would then ask for views it cannot get.
            crate::at_callback_boundary("fif:numberOfRows", 0, || {
                isize::try_from(ROWS.with(|r| r.borrow().len())).unwrap_or(isize::MAX)
            })
        }
    }

    unsafe impl NSControlTextEditingDelegate for TableSource {}

    unsafe impl NSTableViewDelegate for TableSource {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for_column_row(
            &self,
            _table: &NSTableView,
            column: Option<&NSTableColumn>,
            row: isize,
        ) -> Option<Retained<NSView>> {
            crate::at_callback_boundary("fif:viewForRow", None, || {
                let mtm = MainThreadMarker::new()?;
                let column = column?;
                let id = column.identifier().to_string();
                let index = usize::try_from(row).ok()?;
                let text = ROWS.with(|rows| {
                    let rows = rows.borrow();
                    let row = rows.get(index)?;
                    Some(match id.as_str() {
                        COL_LINE => row.line.clone(),
                        COL_MATCH => row.text.clone(),
                        _ => row.file.clone(),
                    })
                })?;
                let label = NSTextField::labelWithString(&NSString::from_str(&text), mtm);
                label.setFont(Some(&NSFont::systemFontOfSize(
                    NSFont::smallSystemFontSize(),
                )));
                label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                Some(Retained::into_super(Retained::into_super(label)))
            })
        }
    }
);

impl TableSource {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `init` on a freshly allocated instance of our own
        // class, which adds no ivars needing other initialisation.
        unsafe { msg_send![this, init] }
    }
}

define_class!(
    // SAFETY: an `NSView` subclass that overrides only mouse tracking and
    // cursor rects. Main-thread-only because every callback arrives there.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppFifDivider"]
    pub struct Divider;

    unsafe impl NSObjectProtocol for Divider {}

    impl Divider {
        /// Paint the handle as a hairline so it reads as a divider
        /// rather than as dead space.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("fif:dividerDrawRect", (), || {
                NSColor::separatorColor().setFill();
                objc2_app_kit::NSRectFill(self.bounds());
            });
        }

        /// Show a vertical-resize cursor over the handle.
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            crate::at_callback_boundary("fif:resetCursorRects", (), || {
                // `resizeUpDownCursor` is deprecated in favour of
                // `rowResizeCursorInDirections:`, which is macOS 15+.
                // Code++ has no such deployment floor, and the
                // replacement's whole benefit is a directional variant
                // this divider does not need — it resizes both ways.
                #[allow(deprecated)]
                let cursor = NSCursor::resizeUpDownCursor();
                self.addCursorRect_cursor(self.bounds(), &cursor);
            });
        }

        /// Accept the drag. AppKit routes the following `mouseDragged:`
        /// and `mouseUp:` to whichever view took the `mouseDown:`.
        ///
        /// **No nested tracking loop here, unlike the tab strip.** That
        /// one has to pump events by hand because `NSButton`'s own
        /// `mouseDown:` runs a tracking loop that does not return until
        /// mouse-up, so an ordinary target/action pair has already lost
        /// the gesture by the time it fires. A plain `NSView` gets normal
        /// event delivery, so none of that — and none of its hazards
        /// (the `DrainFreeze`, the use-after-free from resyncing under a
        /// live `mouseDown:`) applies. This view is also created once and
        /// never rebuilt, so nothing can free it mid-gesture.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            crate::at_callback_boundary("fif:dividerMouseDown", (), || {});
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            crate::at_callback_boundary("fif:dividerMouseDragged", (), || {
                drag_divider_to(event.locationInWindow().y);
            });
        }
    }
);

/// The bottom results dock.
///
/// Built once at startup and held on [`crate::state::CocoaUiState`];
/// hidden until a search produces results. `Clone` is a refcount bump per
/// field — it is handed out by `CocoaUiState::split` on every drain, so
/// it must stay cheap.
#[derive(Clone)]
pub struct FifDock {
    /// The dock's root. The caller positions it; see
    /// `CocoaUi::relayout_chrome`.
    pub container: Retained<NSView>,
    table: Retained<NSTableView>,
    /// Summary / live-progress line in the dock header.
    header: Retained<NSTextField>,
    cancel: Retained<NSButton>,
    /// Held because `NSTableView`'s `dataSource` and `delegate` are both
    /// **weak** — nothing else keeps this alive, and a released data
    /// source is a dangling weak reference AppKit will message.
    #[allow(dead_code)]
    source: Retained<TableSource>,
}

impl FifDock {
    /// Build the dock, sized to `width`. Starts hidden.
    pub fn new(width: f64, actions: &Actions, mtm: MainThreadMarker) -> Self {
        let height = DOCK_DEFAULT_HEIGHT;
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
        );
        // Keeps its height and its distance from the bottom as the window
        // grows; the editor above absorbs the slack.
        container.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        container.setHidden(true);

        // --- the drag handle, along the top edge --------------------
        let divider = Divider::new(
            NSRect::new(
                NSPoint::new(0.0, height - DIVIDER_HEIGHT),
                NSSize::new(width, DIVIDER_HEIGHT),
            ),
            mtm,
        );
        divider.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        container.addSubview(&divider);

        // --- header: summary label (springs) + Cancel + close -------
        let header_y = height - DIVIDER_HEIGHT - HEADER_HEIGHT;
        let close = header_button("✕", 24.0, width - 28.0, header_y, actions, mtm);
        // SAFETY: a compile-time `sel!` literal implemented by `Actions`,
        // whose lifetime the window state owns for the process.
        unsafe { close.setAction(Some(sel!(codeppFifClose:))) };
        let cancel = header_button("Cancel", 76.0, width - 108.0, header_y, actions, mtm);
        // SAFETY: as above.
        unsafe { cancel.setAction(Some(sel!(codeppFifCancel:))) };
        cancel.setEnabled(false);

        let header = NSTextField::labelWithString(&NSString::from_str("Find in Files"), mtm);
        header.setFrame(NSRect::new(
            NSPoint::new(8.0, header_y),
            NSSize::new((width - 124.0).max(0.0), HEADER_HEIGHT),
        ));
        header.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        header.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        // **`ViewMinYMargin` is load-bearing, not decoration.** The header
        // row has to stay pinned to the dock's top edge as the divider
        // makes the dock taller. Without a flexible margin below it,
        // autoresizing has nowhere to put the delta and leaves the view at
        // its original `y` — which at the default height looks perfectly
        // correct and, once dragged taller, buries the header and both
        // buttons underneath the results table. Caught by driving a real
        // drag and dumping the dock's subview frames; nothing about the
        // dock at its opening height reveals it.
        header.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        container.addSubview(&header);
        container.addSubview(&cancel);
        container.addSubview(&close);

        // --- the results table, filling what is left ----------------
        let table_height = (height - DIVIDER_HEIGHT - HEADER_HEIGHT).max(0.0);
        let table = NSTableView::initWithFrame(
            NSTableView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, table_height)),
        );
        table.setRowHeight(ROW_HEIGHT);
        table.setUsesAlternatingRowBackgroundColors(true);
        table.setAllowsMultipleSelection(false);
        add_column(&table, COL_FILE, "File", 340.0, mtm);
        add_column(&table, COL_LINE, "Line", 56.0, mtm);
        add_column(&table, COL_MATCH, "Match", 520.0, mtm);

        let source = TableSource::new(mtm);
        // SAFETY: both properties are weak, which is exactly why `source`
        // is stored on the returned `FifDock` — see the field's comment.
        // The protocol object borrows from a value that outlives the call.
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(&*source)));
            table.setDelegate(Some(ProtocolObject::from_ref(&*source)));
            table.setTarget(Some(&**actions));
            table.setDoubleAction(Some(sel!(codeppFifRowActivated:)));
        }

        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, table_height)),
        );
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(true);
        scroll.setBorderType(NSBorderType::NoBorder);
        // Overlay scrollers — AppKit's default — are right here, unlike
        // the editor's scroll view, which is forced to `Legacy` because
        // Scintilla scrolls its own content and never triggers the
        // overlay's appearance (DESIGN.md §7.4). This is an ordinary
        // AppKit list: a wheel or trackpad scroll flashes them normally.
        scroll.setDocumentView(Some(&table));
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        container.addSubview(&scroll);

        Self {
            container,
            table,
            header,
            cancel,
            source,
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.container.isHidden()
    }

    /// The dock's height for this layout pass: zero while hidden, and
    /// otherwise the persisted height clamped to what the window can
    /// currently give it.
    ///
    /// **The clamp belongs here, not only in the divider drag.** The
    /// persisted height outlives the window size that justified it, and
    /// a plain window resize is handled entirely by autoresizing masks —
    /// the editor is the only flexible view, so it absorbs the whole
    /// delta and a window shrunk below the fixed strips starves it to
    /// nothing while the dock keeps its old height. Clamping at every
    /// layout is what `ui_win32::layout_children` does with its own
    /// `clamp_dock_height`, and `ui_gtk` gets the equivalent for free
    /// because its dock lives in a `GtkPaned` that self-clamps on each
    /// size-allocate. This backend had ported only the drag-time half.
    ///
    /// The persisted value is deliberately **not** written back, also
    /// matching Win32: the clamp is per-layout, so growing the window
    /// again restores the height the user actually dragged rather than
    /// leaving it stuck at whatever the smallest intermediate size
    /// allowed.
    pub fn height_for_layout(&self, content_height: f64, tabs_h: f64, toolbar_h: f64) -> f64 {
        if self.is_hidden() {
            return 0.0;
        }
        clamp_dock_height(
            DOCK_HEIGHT.with(Cell::get),
            content_height,
            tabs_h,
            toolbar_h,
        )
    }

    fn set_header(&self, text: &str) {
        self.header.setStringValue(&NSString::from_str(text));
    }
}

/// A right-aligned header button.
fn header_button(
    title: &str,
    width: f64,
    x: f64,
    y: f64,
    actions: &Actions,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, HEADER_HEIGHT)),
    );
    button.setTitle(&NSString::from_str(title));
    button.setBezelStyle(NSBezelStyle::AccessoryBarAction);
    button.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    // Pinned to the trailing edge, so a horizontal resize opens the gap
    // in the header label rather than under the buttons — and to the top
    // edge, so a *vertical* one keeps them on the header row. See the
    // note on the header label for what the missing `ViewMinYMargin`
    // looked like.
    button.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    // SAFETY: the target is a weak reference to an object the window
    // state owns for the process lifetime. The action is set by the
    // caller, which is where the selector is known.
    unsafe { button.setTarget(Some(&**actions)) };
    button
}

/// Append a resizable text column.
fn add_column(
    table: &NSTableView,
    identifier: &str,
    title: &str,
    width: f64,
    mtm: MainThreadMarker,
) {
    let column = NSTableColumn::initWithIdentifier(
        NSTableColumn::alloc(mtm),
        &NSString::from_str(identifier),
    );
    column.setTitle(&NSString::from_str(title));
    column.setWidth(width);
    column.setMinWidth(40.0);
    table.addTableColumn(&column);
}

impl Divider {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `initWithFrame:` is `NSView`'s designated initialiser
        // and this subclass adds no ivars needing other initialisation.
        let view: Retained<Self> = unsafe { msg_send![this, initWithFrame: frame] };
        view
    }
}

/// Move the divider so the dock's top edge sits at `window_y`.
///
/// Called from the drag; the arithmetic is in [`clamp_dock_height`] so it
/// can be tested without a mouse.
fn drag_divider_to(window_y: f64) {
    let Some((wanted, content_h, tabs_h, toolbar_h)) = with_state(|st| {
        let (_, ui) = st.split();
        let content_h = ui
            .window
            .contentView()
            .map_or(0.0, |c| c.bounds().size.height);
        (
            window_y - crate::status::STATUS_BAR_HEIGHT,
            content_h,
            if ui.tabs.is_hidden() {
                0.0
            } else {
                crate::tabs::TAB_STRIP_HEIGHT
            },
            if ui.toolbar.is_hidden() {
                0.0
            } else {
                crate::toolbar::TOOLBAR_HEIGHT
            },
        )
    }) else {
        return;
    };
    let height = clamp_dock_height(wanted, content_h, tabs_h, toolbar_h);
    if (height - DOCK_HEIGHT.with(Cell::get)).abs() < 0.5 {
        return;
    }
    DOCK_HEIGHT.with(|c| c.set(height));
    with_state(|st| {
        let (_, ui) = st.split();
        ui.relayout_chrome();
    });
}

/// Clamp a dragged dock height to what the window can give it.
///
/// Pure so the boundaries — which is what a divider drag gets wrong —
/// are testable; dragging a real one is exactly the case a hands-on demo
/// is worst at checking. The ceiling is "whatever is left after the
/// status bar, the visible chrome strips and a minimum editor", and it is
/// floored at [`DOCK_MIN_HEIGHT`] so a very short window still yields a
/// usable dock rather than a negative one.
fn clamp_dock_height(wanted: f64, content_height: f64, tabs_h: f64, toolbar_h: f64) -> f64 {
    let ceiling =
        content_height - crate::status::STATUS_BAR_HEIGHT - tabs_h - toolbar_h - EDITOR_MIN_HEIGHT;
    wanted.clamp(DOCK_MIN_HEIGHT, ceiling.max(DOCK_MIN_HEIGHT))
}

/// The inputs [`run_search`] needs, harvested from the Find/Replace
/// panel's Find-in-Files tab before any modal confirm runs.
///
/// The five `bool`s are the dialog's five independent checkbox states
/// (three shared search options + two scan-scope toggles), each a
/// distinct user choice forwarded verbatim into the request — so a
/// flags-struct or enum would only obscure a plain 1:1 UI mirror.
#[allow(clippy::struct_excessive_bools)]
pub struct FifInputs {
    pub query: String,
    /// `Some` for Replace-in-Files, `None` for Find All.
    pub replacement: Option<String>,
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub directory: String,
    pub filters: String,
    pub recurse: bool,
    pub hidden: bool,
}

/// Start a Find-in-Files (or Replace-in-Files) job from harvested inputs.
///
/// Returns `Err(message)` for a bad input (empty query/directory, bad
/// glob, or a rejected start) so the caller can show it on the dialog's
/// status line. On success the dock is reset and shown, and results
/// arrive later through [`drain_into_dock`].
///
/// # Errors
///
/// Surfaces empty-query / empty-directory guards, `set_includes` glob
/// errors, and any `codepp_shell::FifError` from `start_fif`, all as a
/// display string.
pub fn run_search(inputs: FifInputs) -> Result<(), String> {
    if inputs.query.is_empty() {
        return Err("Enter a search term".to_string());
    }
    if inputs.directory.trim().is_empty() {
        return Err("Enter a directory to search".to_string());
    }
    let mut walk = FifWalkOpts::default();
    walk.recurse = inputs.recurse;
    walk.walk_hidden_dirs = inputs.hidden;
    apply_filters(&mut walk, &inputs.filters)?;

    let opts = FifQueryOpts {
        match_case: inputs.match_case,
        whole_word: inputs.whole_word,
        regex: inputs.regex,
    };
    let root = PathBuf::from(inputs.directory.trim());
    let is_replace = inputs.replacement.is_some();

    let outcome = with_state(move |st| {
        let open_tab_paths = st
            .shell
            .tabs
            .iter()
            .filter_map(|t| t.path.clone())
            .collect();
        let request = FifRequest {
            query: inputs.query,
            opts,
            root,
            walk,
            replacement: inputs.replacement,
            open_tab_paths,
        };
        match st.shell.start_fif(request) {
            Ok(job) => {
                st.fif_job.begin(job, is_replace);
                ROWS.with(|r| r.borrow_mut().clear());
                st.fif_dock.table.reloadData();
                st.fif_dock.set_header("Searching…");
                st.fif_dock.cancel.setEnabled(true);
                let was_hidden = st.fif_dock.is_hidden();
                st.fif_dock.container.setHidden(false);
                if was_hidden {
                    let (_, ui) = st.split();
                    ui.relayout_chrome();
                }
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    });
    outcome.unwrap_or_else(|| Err("window is gone".to_string()))
}

/// Cancel the running job — the dock's Cancel button.
///
/// Guarded on there actually being one. The button is disabled between
/// jobs, so today this cannot fire idle; the guard is so a future caller
/// that does not inherit that invariant cannot leave "Cancelling…" on
/// screen for a job that already finished. Same shape as [`close_dock`].
pub(crate) fn cancel_job() {
    with_state(|st| {
        if st.fif_job.active.is_none() {
            return;
        }
        st.shell.cancel_fif();
        st.fif_dock.set_header("Cancelling…");
    });
}

/// Hide the dock, cancelling any still-running job so no orphaned worker
/// keeps posting wakes at an invisible dock.
pub(crate) fn close_dock() {
    with_state(|st| {
        if st.fif_job.active.take().is_some() {
            st.shell.cancel_fif();
        }
        st.fif_dock.container.setHidden(true);
        let (_, ui) = st.split();
        ui.relayout_chrome();
    });
}

/// Translate the whitespace-separated Filters box into include globs.
///
/// `*` / `*.*` / empty mean "all files" (no include set). A bare `*.ext`
/// is anchored with `**/` so it matches at any depth. Mirrors
/// `ui_win32::apply_filters_to_walk_opts` and `ui_gtk::fif::apply_filters`
/// so all three backends read filters identically.
fn apply_filters(walk: &mut FifWalkOpts, filters_text: &str) -> Result<(), String> {
    let mut patterns: Vec<String> = Vec::new();
    for tok in filters_text.split_whitespace() {
        if tok == "*" || tok == "*.*" {
            continue;
        }
        if tok.starts_with('!') {
            return Err("filters: '!' negation is not supported — remove those tokens".to_string());
        }
        patterns.push(if tok.starts_with("**/") {
            tok.to_string()
        } else {
            format!("**/{tok}")
        });
    }
    if patterns.is_empty() {
        return Ok(());
    }
    let refs: Vec<&str> = patterns.iter().map(String::as_str).collect();
    walk.set_includes(&refs).map_err(|e| e.to_string())
}

/// Drain the FIF event queue into the dock. Called from `drain_shell` on
/// every §5.4 wake, after the main `Shell::drain`.
pub(crate) fn drain_into_dock() {
    let terminal = with_state(|st| {
        let Some(active) = st.fif_job.active else {
            // No job running: discard anything still queued rather than
            // letting it accumulate against the next job.
            let _ = st.shell.take_fif_events();
            return None;
        };
        let active = active.raw();
        let mut terminal: Option<Terminal> = None;
        for ev in st.shell.take_fif_events() {
            match ev {
                FifEvent::FileMatches { job, path, outcome } if job.raw() == active => {
                    st.fif_job.hit_files += 1;
                    st.fif_job.hit_matches += outcome.matches.len();
                    st.fif_job.pending.push(PendingFile {
                        path,
                        matches: outcome.matches,
                        truncated: outcome.truncated,
                    });
                }
                FifEvent::ReplacedInOpenBuffer {
                    job,
                    replaced,
                    outcome,
                    ..
                } if job.raw() == active => match outcome {
                    OpenBufferOutcome::Replaced => st.fif_job.replaced_in_buffers += replaced,
                    OpenBufferOutcome::SkippedDirty => st.fif_job.skipped_dirty += 1,
                    OpenBufferOutcome::TabClosed => {}
                },
                FifEvent::Done { job, stats } if job.raw() == active => {
                    terminal = Some(Terminal::Done(stats));
                }
                FifEvent::Cancelled { job } if job.raw() == active => {
                    terminal = Some(Terminal::Cancelled);
                }
                // Stale-job events, and `ReplaceInOpenBuffer` (consumed by
                // `Shell::drain` before it ever reaches here) are ignored.
                _ => {}
            }
        }
        // Live progress while the job is still running.
        if terminal.is_none() {
            let (f, m) = (st.fif_job.hit_files, st.fif_job.hit_matches);
            st.fif_dock
                .set_header(&format!("Searching… {m} matches in {f} files"));
        }
        terminal
    })
    .flatten();

    if let Some(term) = terminal {
        with_state(|st| finalize(st, &term));
    }
    apply_pending_jump();
}

/// Flush the buffered matches into the table and write the summary.
fn finalize(st: &mut crate::state::CocoaUiState, term: &Terminal) {
    let mut rows: Vec<ResultRow> = Vec::new();
    for file in &st.fif_job.pending {
        let mut file_display = sanitize_path_for_display(&file.path);
        if file.truncated {
            file_display.push_str(" (truncated)");
        }
        for m in &file.matches {
            rows.push(ResultRow {
                file: file_display.clone(),
                line: m.line_no.to_string(),
                // Sanitized: a match line is arbitrary file content, and
                // it renders straight into an `NSTextField`.
                text: sanitize_str_for_display(&m.line_text),
                jump: JumpTarget {
                    // The **real** path, not the display one — see
                    // `ResultRow`.
                    path: file.path.clone(),
                    line_no: m.line_no,
                    col_start: m.col_start,
                    col_end: m.col_end,
                },
            });
        }
    }
    ROWS.with(|r| *r.borrow_mut() = rows);
    st.fif_dock.table.reloadData();
    st.fif_dock.set_header(&st.fif_job.summary(term));
    st.fif_dock.cancel.setEnabled(false);
    st.fif_job.active = None;
    st.fif_job.pending.clear();
}

/// A results row was double-clicked: queue the jump and open the file.
pub(crate) fn row_activated(row: isize) {
    let Ok(index) = usize::try_from(row) else {
        // `clickedRow` is -1 when the double-click landed on the header
        // or on empty space below the last row.
        return;
    };
    let Some(target) = ROWS.with(|rows| rows.borrow().get(index).map(|r| r.jump.clone())) else {
        return;
    };

    with_state(|st| st.fif_job.pending_jump = Some(target.clone()));
    let outcome = with_state(|st| st.shell.open_file(target.path.clone()));
    match outcome {
        // `SwitchedToExisting` queues no load, so nothing would rebind the
        // single Scintilla view — the tab strip would show the switch
        // while the editor kept rendering the previous buffer. See
        // `OpenFileOutcome`'s own docs, and DESIGN.md §7.4, where
        // discarding this return value is recorded as a shipped bug.
        Some(OpenFileOutcome::SwitchedToExisting(_)) => {
            crate::rebind_active_view();
            apply_pending_jump();
        }
        Some(OpenFileOutcome::AlreadyActive) => apply_pending_jump(),
        // `Loading`: the jump applies when the load lands via
        // `drain_into_dock`. Anything else: drop it.
        Some(OpenFileOutcome::Loading) => {}
        _ => {
            with_state(|st| st.fif_job.pending_jump = None);
        }
    }
}

/// Apply the queued jump if the active tab is now the file it targets.
///
/// A no-op while the file is still loading (the active tab's path will
/// not match yet) — the next `drain_into_dock` after the load binds the
/// document retries it. Selects the match span so the hit is highlighted,
/// not merely scrolled into view.
fn apply_pending_jump() {
    let jumped = with_state(|st| {
        let Some(jump) = st.fif_job.pending_jump.clone() else {
            return false;
        };
        let active_path = st
            .shell
            .active_tab
            .and_then(|i| st.shell.tabs.get(i))
            .and_then(|t| t.path.clone());
        if active_path.as_deref() != Some(jump.path.as_path()) {
            return false;
        }
        let line0 = jump.line_no.saturating_sub(1) as usize;
        // Brings the line into view.
        st.editor.send(SCI_GOTOLINE, line0, 0);
        let line_start = st.editor.send(SCI_POSITIONFROMLINE, line0, 0).max(0) as usize;
        // The columns are UTF-8 byte offsets within the line — the same
        // units Scintilla positions use, so no re-encoding.
        let sel_start = line_start + jump.col_start as usize;
        let sel_end = line_start + jump.col_end as usize;
        st.editor.send(SCI_SETSEL, sel_start, sel_end as isize);
        st.fif_job.pending_jump = None;
        true
    })
    .unwrap_or(false);
    if jumped {
        // The caret and the selection both moved, and `SCN_UPDATEUI`
        // arrives on Scintilla's next pass — refresh now rather than
        // relying on that timing, the same call every `crate::search`
        // command makes after a borrow that moved the caret.
        crate::search::refresh_chrome();
    }
}

/// Source-level guards for the two call sites that make the dock's
/// height clamp *authoritative* rather than drag-only.
///
/// **Why a source scan.** The property is "the persisted height is
/// re-clamped on every layout", and its failure is a window that looks
/// fine until the user shrinks it far enough — no crash, no wrong return
/// value, nothing a headless test can observe, because the frames only
/// go wrong once a real window server has resized a real window. This
/// shipped once in review: only the drag path clamped, so a
/// resize-down starved the editor while the dock kept its old height.
/// The same tool `ui_gtk` uses for its single-view invariant and
/// `ui_cocoa` already uses for window activation, for the same reason.
#[cfg(test)]
mod source_invariants {
    /// The body of `fn name` in `src`, by brace matching.
    fn fn_body(src: &str, name: &str) -> String {
        let sig = format!("fn {name}(");
        let start = src.find(&sig).unwrap_or_else(|| panic!("no fn {name}"));
        let open = src[start..].find('{').expect("no body") + start;
        let mut depth = 0usize;
        for (i, c) in src[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return src[open..=open + i].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated body for {name}");
    }

    /// `relayout_chrome` must take the dock's height through the
    /// clamping accessor, never from the stored value directly.
    #[test]
    fn relayout_clamps_the_dock_height() {
        let body = fn_body(include_str!("platform.rs"), "relayout_chrome");
        assert!(
            body.contains("height_for_layout("),
            "relayout_chrome must clamp the dock height via \
             FifDock::height_for_layout; an unclamped read lets a \
             window-resize-down starve the editor"
        );
    }

    /// And a window resize must re-run that layout — autoresizing masks
    /// alone give the whole delta to the editor.
    #[test]
    fn a_window_resize_relays_the_chrome_bands() {
        let body = fn_body(include_str!("menu.rs"), "window_did_resize");
        assert!(
            body.contains("relayout_chrome_bands()"),
            "windowDidResize: must re-lay the chrome bands, or the dock \
             keeps a height the shrunken window cannot afford"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_dock_height, DOCK_MIN_HEIGHT, EDITOR_MIN_HEIGHT};

    /// A roomy window gives the user exactly what they dragged.
    #[test]
    fn an_ordinary_drag_is_honoured() {
        assert!((clamp_dock_height(300.0, 900.0, 26.0, 30.0) - 300.0).abs() < f64::EPSILON);
    }

    /// Dragging down past the floor stops at it, rather than collapsing
    /// the dock to a sliver the header cannot fit in.
    #[test]
    fn dragging_below_the_floor_stops_at_it() {
        assert!(
            (clamp_dock_height(10.0, 900.0, 26.0, 30.0) - DOCK_MIN_HEIGHT).abs() < f64::EPSILON
        );
        assert!(
            (clamp_dock_height(-500.0, 900.0, 26.0, 30.0) - DOCK_MIN_HEIGHT).abs() < f64::EPSILON
        );
    }

    /// Dragging up leaves the editor at least `EDITOR_MIN_HEIGHT`.
    #[test]
    fn dragging_up_leaves_the_editor_usable() {
        let content = 900.0;
        let (tabs, toolbar) = (26.0, 30.0);
        let height = clamp_dock_height(10_000.0, content, tabs, toolbar);
        let editor = content - crate::status::STATUS_BAR_HEIGHT - tabs - toolbar - height;
        assert!(
            editor >= EDITOR_MIN_HEIGHT - 0.001,
            "editor would be {editor} pt, below the {EDITOR_MIN_HEIGHT} pt minimum"
        );
    }

    /// A window too short to satisfy both minimums still yields a dock at
    /// its floor rather than a negative height, which would make
    /// `setFrame:` throw the layout out entirely.
    #[test]
    fn a_window_too_short_for_both_still_yields_the_floor() {
        for content in [0.0, 60.0, 150.0] {
            let height = clamp_dock_height(400.0, content, 26.0, 30.0);
            assert!(
                (height - DOCK_MIN_HEIGHT).abs() < f64::EPSILON,
                "content {content} gave {height}"
            );
        }
    }

    /// Hidden chrome gives its space to the dock's ceiling — the drag
    /// limit has to follow what is actually on screen, not a fixed guess.
    #[test]
    fn hidden_chrome_raises_the_ceiling() {
        let with_chrome = clamp_dock_height(10_000.0, 900.0, 26.0, 30.0);
        let without = clamp_dock_height(10_000.0, 900.0, 0.0, 0.0);
        assert!(
            without > with_chrome,
            "hiding 56 pt of chrome should raise the ceiling: {with_chrome} vs {without}"
        );
    }
}
