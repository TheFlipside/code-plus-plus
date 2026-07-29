//! The toolbar: 32 buttons in 10 separator-delimited groups, mirroring
//! `ui_win32`'s and `ui_gtk`'s layout button-for-button.
//!
//! # Why a plain view rather than `NSToolbar`
//!
//! `NSToolbar` is the native macOS answer and is deliberately not used.
//! It lives in the window's *titlebar*, owns its own item model (string
//! identifiers resolved through a delegate) and its own overflow and
//! customisation UI. Code++'s toolbar is a bar in the content area
//! between the menu and the tab strip, and its layout is the one
//! Notepad++ users already know — which the m1 convention decision keeps
//! wherever macOS does not mandate otherwise, and toolbar *placement* is
//! not something macOS mandates. Composing `NSButton`s in an `NSView` is
//! also what the tab strip does, so the two chrome strips work the same
//! way.
//!
//! # Icons
//!
//! The 24 px (`@1x`) and 48 px (`@2x`) PNGs under `assets/icons/` are
//! embedded with `include_bytes!`, so the binary stays self-contained
//! (DESIGN.md §9.1) and all three backends show one icon set. Both
//! representations go into a single `NSImage` and AppKit picks per
//! display — the same handling the tab strip's save glyph gets, and the
//! reason it keeps working when the window moves between a Retina and a
//! non-Retina display mid-session.
//!
//! # Buttons whose feature is not here yet
//!
//! Every button in Win32's layout is present, and the ones whose feature
//! this backend does not have (search, print, the panel toggles, macros,
//! monitoring, sync-scroll, Define Language) are **greyed** rather than
//! omitted. Same call `ui_gtk` makes: the bar matches Win32 exactly, and
//! a user never gets a dead click. They light up as their milestones
//! land.
//!
//! Greying here is an explicit `setEnabled(false)`, which is *not* how
//! the menu does it — see [`Bar::push`] for why the two cannot share the
//! same mechanism.
//!
//! # Toggle state
//!
//! Word Wrap, Show All Characters and Show Indent Guide track the live
//! editor, and can be changed from the View menu as well as from here.
//! [`Toolbar::refresh_toggles`] re-reads the model and repaints all
//! three, and is called from the one place that mutates a View setting —
//! so the toolbar and the menu can never disagree. The View menu needs
//! no equivalent registration because its marks are resolved on open by
//! `validateMenuItem:`.

use objc2::rc::Retained;
use objc2::{sel, AnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezelStyle, NSBitmapImageRep, NSBox, NSBoxType, NSButton,
    NSButtonType, NSImage, NSImageRep, NSView,
};
use objc2_foundation::{MainThreadMarker, NSData, NSPoint, NSRect, NSSize, NSString};

use crate::menu::Actions;

/// Height of the bar in points.
pub const TOOLBAR_HEIGHT: f64 = 30.0;

/// Logical edge of an icon. The `@2x` asset is twice this; pinning the
/// logical square makes both scales occupy the same cell.
const ICON_LOGICAL_PX: f64 = 24.0;
/// Edge of a button's clickable cell, leaving a little padding round the
/// icon.
const BUTTON_SIZE: f64 = 26.0;
/// Horizontal gap a group separator occupies.
const SEPARATOR_WIDTH: f64 = 9.0;
/// Width of the hairline drawn inside a separator's gap.
const SEPARATOR_LINE_WIDTH: f64 = 1.0;
/// Inset from the bar's left edge, and between buttons.
const BUTTON_GAP: f64 = 2.0;

/// `(png_1x, png_2x)` for one icon, embedded from `assets/icons/`.
macro_rules! icon {
    ($name:literal) => {
        (
            include_bytes!(concat!("../../../assets/icons/", $name, ".png")).as_slice(),
            include_bytes!(concat!("../../../assets/icons/", $name, "@2x.png")).as_slice(),
        )
    };
}

type IconPair = (&'static [u8], &'static [u8]);

/// Handle to the toolbar.
///
/// `Clone` is a refcount bump per field — this is read on every drain, so
/// it stays cheap, like [`crate::tabs::TabStrip`].
#[derive(Clone)]
pub struct Toolbar {
    pub container: Retained<NSView>,
    /// The three toggles that track editor state, in View-toggle tag
    /// order (word wrap, show-all-characters, indent guide). Held so
    /// [`Self::refresh_toggles`] can repaint them when the setting is
    /// changed from the View menu.
    ///
    /// Show All Characters is one button for two settings — whitespace
    /// *and* EOL together, matching Win32's combined button — so it has
    /// no single tag and is handled explicitly.
    toggles: Vec<Retained<NSButton>>,
}

impl Toolbar {
    /// Build the bar in Win32's exact 10-group order.
    pub fn new(width: f64, actions: &Actions, mtm: MainThreadMarker) -> Self {
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, TOOLBAR_HEIGHT)),
        );
        container.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);

        let mut bar = Bar {
            container: &container,
            actions,
            mtm,
            x: BUTTON_GAP,
        };
        let mut toggles = Vec::new();

        // Group 1 — file operations.
        bar.push(icon!("new"), "New", Some(sel!(codeppNewFile:)));
        bar.push(icon!("open"), "Open…", Some(sel!(codeppOpenFile:)));
        bar.push(icon!("save"), "Save", Some(sel!(codeppSaveFile:)));
        bar.push(icon!("save-all"), "Save All", Some(sel!(codeppSaveAll:)));
        bar.push(icon!("close"), "Close", Some(sel!(codeppCloseCurrentTab:)));
        bar.push(icon!("close-all"), "Close All", Some(sel!(codeppCloseAll:)));
        // Print needs an `NSPrintOperation` driving Scintilla's
        // `SCI_FORMATRANGEFULL`; greyed here as it is in the File menu.
        bar.push(icon!("print"), "Print", None);
        bar.separator();

        // Group 2 — clipboard. Standard AppKit selectors with a nil
        // target, so they travel the responder chain to the Scintilla
        // view exactly as the Edit menu's items do.
        bar.responder(icon!("cut"), "Cut", sel!(cut:));
        bar.responder(icon!("copy"), "Copy", sel!(copy:));
        bar.responder(icon!("paste"), "Paste", sel!(paste:));
        bar.separator();

        // Group 3 — history. Always enabled: a no-op when there is
        // nothing to undo, and the dynamic grey-out Win32 does is a
        // tracked follow-up there too. Responder-chain again, so these
        // reach Scintilla's own `undo:`/`redo:`.
        bar.responder(icon!("undo"), "Undo", sel!(undo:));
        bar.responder(icon!("redo"), "Redo", sel!(redo:));
        bar.separator();

        // Group 4 — search. Greyed until the dialogs land in m4, matching
        // the Search menu.
        bar.push(icon!("find"), "Find…", None);
        bar.push(icon!("replace"), "Replace…", None);
        bar.separator();

        // Group 5 — zoom.
        bar.push(
            icon!("zoom-in"),
            "Zoom In (⌘ + Mouse Wheel Up)",
            Some(sel!(codeppZoomIn:)),
        );
        bar.push(
            icon!("zoom-out"),
            "Zoom Out (⌘ + Mouse Wheel Down)",
            Some(sel!(codeppZoomOut:)),
        );
        bar.separator();

        // Group 6 — sync scroll. Needs a second view; not on this
        // backend, and not on GTK either.
        bar.disabled_toggle(
            icon!("sync-scroll-vertical"),
            "Synchronize Vertical Scrolling",
        );
        bar.disabled_toggle(
            icon!("sync-scroll-horizontal"),
            "Synchronize Horizontal Scrolling",
        );
        bar.separator();

        // Group 7 — the three functional view toggles.
        toggles.push(bar.toggle(
            icon!("word-wrap"),
            "Word Wrap",
            sel!(codeppToolbarWordWrap:),
        ));
        toggles.push(bar.toggle(
            icon!("show-all-chars"),
            "Show All Characters",
            sel!(codeppToolbarShowAllChars:),
        ));
        toggles.push(bar.toggle(
            icon!("show-indent-guide"),
            "Show Indent Guide",
            sel!(codeppToolbarIndentGuide:),
        ));
        bar.separator();

        // Group 8 — tools and panels. The workspace tree, document map,
        // document list and function list are all m4.
        bar.push(icon!("define-language"), "Define your language…", None);
        bar.disabled_toggle(icon!("document-map"), "Document Map");
        bar.disabled_toggle(icon!("document-list"), "Document List");
        bar.disabled_toggle(icon!("function-list"), "Function List");
        bar.disabled_toggle(icon!("folder-workspace"), "Folder as Workspace");
        bar.separator();

        // Group 9 — monitoring.
        bar.push(icon!("monitoring"), "Monitoring (tail -f)", None);
        bar.separator();

        // Group 10 — macros.
        bar.push(icon!("macro-record"), "Start Recording", None);
        bar.push(icon!("macro-stop"), "Stop Recording", None);
        bar.push(icon!("macro-play"), "Playback", None);
        bar.push(icon!("run"), "Run a Macro Multiple Times…", None);
        bar.push(icon!("save-macro"), "Save Current Recorded Macro…", None);

        Self { container, toggles }
    }

    /// Repaint the three functional toggles from the persisted View
    /// settings.
    ///
    /// Called after every View-setting mutation, from whichever surface
    /// made it, so the toolbar and the View menu cannot drift. Reads the
    /// model rather than flipping the button's own state, which is the
    /// same "never infer state from the control" rule the tab strip's
    /// selection follows.
    ///
    /// Show All Characters is on only when *both* the whitespace and EOL
    /// settings are — it is one button for two settings, matching Win32's
    /// combined button, so "partly on" has to resolve one way and off is
    /// the honest one.
    pub fn refresh_toggles(&self) {
        let Some(view) = crate::view_settings() else {
            // Unreadable state. Leave the buttons as they are rather than
            // painting "all off" from a read that never happened.
            return;
        };
        // `zip` truncates rather than complaining, so a future edit that
        // adds a fourth `bar.toggle(...)` without extending the array
        // below would silently stop refreshing it. Pin the pairing.
        debug_assert_eq!(
            self.toggles.len(),
            3,
            "the toggle list and the state array below must stay in step"
        );
        for (button, on) in self.toggles.iter().zip([
            view.word_wrap,
            view.show_whitespace && view.show_eol,
            view.indent_guide,
        ]) {
            button.setState(isize::from(on));
        }
    }

    /// Whole-bar visibility, for `NPPM_HIDETOOLBAR` and the View menu.
    pub fn set_hidden(&self, hidden: bool) {
        self.container.setHidden(hidden);
    }

    pub fn is_hidden(&self) -> bool {
        self.container.isHidden()
    }
}

/// Build-time cursor over the bar: tracks the next x position so each
/// `push`/`toggle`/`separator` call reads as one line, the way
/// `ui_gtk`'s `insert(&button, -1)` does.
struct Bar<'a> {
    container: &'a NSView,
    actions: &'a Actions,
    mtm: MainThreadMarker,
    x: f64,
}

impl Bar<'_> {
    /// A push button. `action` of `None` greys it — its feature is not
    /// wired on this backend yet.
    fn push(&mut self, icons: IconPair, tip: &str, action: Option<objc2::runtime::Sel>) {
        let button = self.button(icons, tip);
        // **Explicitly disabled, unlike the menu's placeholders.**
        // `menu::add_disabled` relies on AppKit's automatic *menu* item
        // enabling, which greys an item whose action has no implementor.
        // There is no equivalent for a control: `NSButton` has no
        // automatic enabling at all, so a button with no action stays
        // fully live-looking and simply does nothing when clicked — a
        // dead click, which is the exact thing listing these greyed is
        // meant to avoid. Measured, not assumed: the first version of
        // this file omitted the call and all 32 buttons reported
        // `isEnabled() == true`.
        match action {
            Some(action) => self.target(&button, action),
            None => button.setEnabled(false),
        }
        self.place(&button);
    }

    /// A push button whose selector goes to the **responder chain**
    /// rather than to `Actions` — so it lands on whichever view is first
    /// responder, which for the clipboard and history commands is the
    /// Scintilla view. Same mechanism the Edit menu uses, and the reason
    /// those items need no code of ours.
    fn responder(&mut self, icons: IconPair, tip: &str, action: objc2::runtime::Sel) {
        let button = self.button(icons, tip);
        // SAFETY: `setAction:` with no target requests responder-chain
        // dispatch; the selector is a compile-time `sel!` literal that
        // Scintilla's content view implements (`ScintillaView.mm`'s
        // `cut:`/`copy:`/`paste:`/`undo:`/`redo:`).
        unsafe { button.setAction(Some(action)) };
        self.place(&button);
    }

    /// A functional toggle, returned so the caller can keep it for state
    /// refresh.
    fn toggle(
        &mut self,
        icons: IconPair,
        tip: &str,
        action: objc2::runtime::Sel,
    ) -> Retained<NSButton> {
        let button = self.button(icons, tip);
        button.setButtonType(NSButtonType::PushOnPushOff);
        self.target(&button, action);
        self.place(&button);
        button
    }

    /// A toggle whose feature is not on this backend. Kept a toggle
    /// rather than made a push so it looks like its Win32 counterpart.
    fn disabled_toggle(&mut self, icons: IconPair, tip: &str) {
        let button = self.button(icons, tip);
        button.setButtonType(NSButtonType::PushOnPushOff);
        button.setEnabled(false);
        self.place(&button);
    }

    /// A group separator: a gap with a hairline down the middle.
    ///
    /// An `NSBox` in separator mode rather than a hand-filled view. It is
    /// AppKit's own separator, so it draws the system hairline at the
    /// system width and follows the appearance into dark mode with no
    /// colour of ours to keep in step — and it avoids pulling
    /// `objc2-quartz-core` in just to set one layer background.
    fn separator(&mut self) {
        let line = NSBox::initWithFrame(
            NSBox::alloc(self.mtm),
            NSRect::new(
                NSPoint::new(self.x + (SEPARATOR_WIDTH - SEPARATOR_LINE_WIDTH) / 2.0, 4.0),
                NSSize::new(SEPARATOR_LINE_WIDTH, TOOLBAR_HEIGHT - 8.0),
            ),
        );
        line.setBoxType(NSBoxType::Separator);
        self.container.addSubview(&line);
        self.x += SEPARATOR_WIDTH;
    }

    /// The parts every button shares: frame, icon, tooltip, flat bezel.
    fn button(&self, icons: IconPair, tip: &str) -> Retained<NSButton> {
        let frame = NSRect::new(
            NSPoint::new(self.x, (TOOLBAR_HEIGHT - BUTTON_SIZE) / 2.0),
            NSSize::new(BUTTON_SIZE, BUTTON_SIZE),
        );
        let button = NSButton::initWithFrame(NSButton::alloc(self.mtm), frame);
        // Image-only: an empty title, or AppKit reserves room for one.
        button.setTitle(&NSString::from_str(""));
        button.setToolTip(Some(&NSString::from_str(tip)));
        button.setBezelStyle(NSBezelStyle::AccessoryBar);
        button.setBordered(false);
        if let Some(image) = decode_icon(icons) {
            button.setImage(Some(&image));
        }
        button
    }

    /// Point a button's action at [`Actions`].
    fn target(&self, button: &NSButton, action: objc2::runtime::Sel) {
        // SAFETY: the selector is a compile-time `sel!` literal declared
        // in `Actions`'s own `define_class!` block, so it cannot go stale,
        // and the target is a weak reference to an object the window
        // state owns for the process lifetime.
        unsafe {
            button.setTarget(Some(self.actions));
            button.setAction(Some(action));
        }
    }

    /// Add `button` and advance the cursor past it.
    fn place(&mut self, button: &NSButton) {
        self.container.addSubview(button);
        self.x += BUTTON_SIZE + BUTTON_GAP;
    }
}

/// Decode an icon pair into one `NSImage` carrying both scales.
///
/// `None` on a decode failure: a button without its icon is a cosmetic
/// loss, and taking the process down because a bundled asset would not
/// parse is not proportionate. Both other backends degrade the same way.
fn decode_icon(icons: IconPair) -> Option<Retained<NSImage>> {
    let image = NSImage::initWithSize(
        NSImage::alloc(),
        NSSize::new(ICON_LOGICAL_PX, ICON_LOGICAL_PX),
    );
    let mut added = 0usize;
    for bytes in [icons.0, icons.1] {
        let data = NSData::with_bytes(bytes);
        let Some(rep) = NSBitmapImageRep::imageRepWithData(&data) else {
            continue;
        };
        let rep: Retained<NSImageRep> = Retained::into_super(rep);
        image.addRepresentation(&rep);
        added += 1;
    }
    if added == 0 {
        tracing::warn!("toolbar icon decode failed; button renders without it");
        return None;
    }
    Some(image)
}
