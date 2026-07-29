//! The tab strip.
//!
//! # It is a selector, not a container
//!
//! The one Scintilla view is the strip's **sibling**, not its child.
//! Clicking a tab does not swap views — it rebinds the single view to
//! that tab's document with `SCI_SETDOCPOINTER`. That is the same model
//! Win32 and GTK use, and on this backend it is load-bearing rather than
//! merely consistent: `EditorHandle` is `Copy` with no lifetime, so a
//! per-tab `NSView` would reintroduce exactly the dangling-pointer
//! problem DESIGN.md §7.4 names when it says "a `ui_cocoa` that reaches
//! for one `NSView` per tab inherits the problem the single-view model
//! avoids".
//!
//! # Why `NSButton`s rather than custom drawing
//!
//! `NSTabView` is unsuitable (it owns per-tab content views, which is
//! precisely the model above rules out) and a fully custom-drawn strip
//! means hand-rolling hit-testing, hover states, text truncation and
//! accessibility. Composing each tab from controls is what `ui_gtk` does
//! too — its strip is a `GtkNotebook` carrying label widgets and close
//! buttons, not a drawing surface — so this stays parallel to the
//! backend it is ported from and inherits native look, keyboard
//! accessibility and hover feedback for free.
//!
//! Each tab is a push-on/push-off `NSButton` carrying the buffer's
//! display name, plus a small close button. Both carry the buffer's
//! **`Tab.id` in their `tag`**, never a vector index — see
//! [`TabStrip::sync`] for why that distinction matters.

use codepp_shell::Tab;
use objc2::rc::Retained;
use objc2::{sel, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezelStyle, NSButton, NSButtonType, NSFont, NSLineBreakMode,
    NSView,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use crate::menu::Actions;

/// Height of the strip in points.
pub const TAB_STRIP_HEIGHT: f64 = 26.0;

/// Width budget for one tab, before the close button.
const TAB_WIDTH: f64 = 150.0;
/// Width of the close button at the trailing edge of each tab.
const CLOSE_WIDTH: f64 = 18.0;

/// Handle to the strip.
///
/// `Clone` is a refcount bump on the container — this is stored on the
/// window state and read on every drain, so it must stay cheap.
#[derive(Clone)]
pub struct TabStrip {
    pub container: Retained<NSView>,
}

impl TabStrip {
    pub fn new(width: f64, mtm: MainThreadMarker) -> Self {
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, TAB_STRIP_HEIGHT)),
        );
        container.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        Self { container }
    }

    /// Rebuild the strip from the shell's tab list.
    ///
    /// Rebuilding wholesale rather than diffing is deliberate: the strip
    /// is at most a few dozen small controls, it is refreshed only on
    /// real model changes (open, close, save, reorder, tab switch) and
    /// never per keystroke, and a diffing implementation would need its
    /// own identity bookkeeping — which is the exact thing that has gone
    /// wrong on this project before (DESIGN.md §7.4's tab arm/commit
    /// race). Cheap and obviously-correct beats clever here.
    ///
    /// **Every control carries `Tab.id` in its `tag`, never a vector
    /// index.** §7.4 records a Win32 bug where a close armed on an index
    /// committed against a different buffer after an intervening
    /// keyboard-driven reorder. Ids are allocated monotonically without
    /// reuse, so a stale tag resolves to "gone" rather than to somebody
    /// else's buffer.
    pub fn sync(
        &self,
        tabs: &[Tab],
        active: Option<usize>,
        actions: &Actions,
        mtm: MainThreadMarker,
    ) {
        // Drop the previous generation of controls.
        for view in self.container.subviews() {
            view.removeFromSuperview();
        }

        let active_id = active.and_then(|i| tabs.get(i)).map(|t| t.id);
        let mut x = 0.0;
        for tab in tabs {
            let selected = Some(tab.id) == active_id;
            let button = make_tab_button(tab, selected, x, actions, mtm);
            self.container.addSubview(&button);

            let close = make_close_button(tab.id, x + TAB_WIDTH - CLOSE_WIDTH, actions, mtm);
            self.container.addSubview(&close);

            x += TAB_WIDTH;
        }
    }

    /// Whole-strip visibility, for `NPPM_HIDETABBAR` and the View menu.
    pub fn set_hidden(&self, hidden: bool) {
        self.container.setHidden(hidden);
    }

    pub fn is_hidden(&self) -> bool {
        self.container.isHidden()
    }
}

/// One tab body button.
fn make_tab_button(
    tab: &Tab,
    selected: bool,
    x: f64,
    actions: &Actions,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    // `tab_display_name` is the shared, already-sanitized display name —
    // the same helper the window title uses, so a filename carrying
    // bidi-control or other hostile characters cannot reach the strip
    // raw. DESIGN.md §7.4 records two prior incidents of exactly that
    // class (the Win32 workspace tree, `NPPM_SETSTATUSBAR`).
    let name = codepp_shell::tab_display_name(tab);
    // A leading bullet is the dirty marker. Win32 draws a red/blue save
    // icon and GTK decorates its label; a glyph is the same information
    // in the idiom this strip already uses.
    let title = if tab.dirty {
        format!("● {name}")
    } else {
        format!("  {name}")
    };

    let frame = NSRect::new(
        NSPoint::new(x, 0.0),
        NSSize::new(TAB_WIDTH, TAB_STRIP_HEIGHT),
    );
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    button.setTitle(&NSString::from_str(&title));
    // `AccessoryBar` is the non-deprecated spelling of what used to
    // be `Recessed`: a flat, tinted-when-on bezel, which is the right
    // look for a tab.
    button.setBezelStyle(NSBezelStyle::AccessoryBar);
    // SAFETY: plain AppKit setters on a live, freshly-allocated button,
    // on the main thread (proven by `mtm`). The selector is a
    // compile-time `sel!` literal that `Actions` implements, and the
    // target is a weak reference AppKit zeroes if it ever outlives the
    // receiver — which it cannot here, since the window state owns
    // `Actions` for the process lifetime.
    unsafe {
        button.setButtonType(NSButtonType::PushOnPushOff);
        button.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        button.setTag(tab.id as isize);
        button.setTarget(Some(actions));
        button.setAction(Some(sel!(codeppSelectTab:)));
        if let Some(cell) = button.cell() {
            cell.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
        }
    }
    // `NSControlStateValueOn` is 1, `Off` is 0.
    button.setState(isize::from(selected));
    button
}

/// The close button for one tab.
fn make_close_button(
    id: i32,
    x: f64,
    actions: &Actions,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let frame = NSRect::new(
        NSPoint::new(x, 4.0),
        NSSize::new(CLOSE_WIDTH, TAB_STRIP_HEIGHT - 8.0),
    );
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    button.setTitle(&NSString::from_str("×"));
    // SAFETY: as in `make_tab_button` above — AppKit setters on a live
    // button on the main thread, with a `sel!` literal `Actions`
    // implements and a weak target that outlives every button.
    unsafe {
        button.setBordered(false);
        button.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        button.setTag(id as isize);
        button.setTarget(Some(actions));
        button.setAction(Some(sel!(codeppCloseTab:)));
    }
    button
}
