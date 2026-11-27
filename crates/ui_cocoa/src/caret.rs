//! The editor's caret, drawn as a layer of its own rather than by
//! Scintilla, so a blink costs no window drawable (DESIGN.md §8,
//! "macOS 27.0.1").
//!
//! # Why
//!
//! Scintilla blinks its caret by invalidating the caret's rect twice a
//! second. On macOS 27 every such redraw of an active window with
//! coloured content makes the system keep a second full-window
//! drawable and a GPU context alive: about 16 MB of the 57 MB the app
//! reads with one empty buffer. A minimal Cocoa window does the same,
//! so it is the platform's price for redrawing view content on a timer.
//! Toggling a sublayer's `hidden` is applied by the compositor and
//! redraws nothing. Measured with the caret blinking: 29.5 MB for a
//! window not yet edited, 41.3 MB once something has been typed.
//!
//! # What Code++ draws, and what it leaves to Scintilla
//!
//! Only the case a one-colour rectangle reproduces exactly: a line
//! caret, one selection, insert mode, no virtual space, no
//! bidirectional layout, the selection not hidden, a caret width above
//! 0, no IME composition and no mouse button down (so a drag's drop
//! caret is Scintilla's). Nor while the layer cannot be placed: the
//! content view has no layer yet, or Scintilla's x cannot be read.
//! Then Scintilla's caret is made invisible and steady — the style's
//! insert shape set to `CARETSTYLE_INVISIBLE`, the period to 0 — and a
//! `CALayer` on `SCIContentView`'s layer takes its place: at the device
//! pixel `EditView::DrawCarets` would use, from the float x Scintilla
//! computes (`scintilla_cocoa_location_x`; `SCI_POINTXFROMPOSITION`
//! truncates it, which is a point out at most positions), with the
//! line's height, the caret width and the caret colour, blinking at
//! Scintilla's period and shown again whenever the caret moves or the
//! view regains the focus, as Scintilla's is. In every other case
//! Scintilla's own style and period are put back and it draws the caret
//! itself, so block and overtype carets, rectangular and multiple
//! selections, IME and the drop caret behave exactly as before.
//!
//! The decision is made again on every `SCN_UPDATEUI`, `SCN_PAINTED`,
//! `SCN_FOCUSIN` and `SCN_FOCUSOUT` of the main view, and on every
//! blink tick, which is what notices a mouse button let go.
//!
//! # Plugins
//!
//! A plugin's `SCI_SETCARETSTYLE` / `SCI_SETCARETPERIOD` to the main
//! view, and their getters, are answered here ([`intercept`]), so a
//! plugin sets and reads the values it would without the layer caret.
//! A plugin that calls Scintilla through its direct-call pointer
//! bypasses that: a style it sets is noticed at the next sync and
//! adopted, but a period of 0 set that way while Code++ draws the caret
//! cannot be told from Code++'s own and leaves the layer blinking. So
//! does a message that arrives while the caret's state is in use, which
//! goes straight to Scintilla and is adopted the same way.
//!
//! Every function here runs on the main thread and takes no
//! `with_state` borrow, so it can be called from Scintilla's
//! notification callback inside one.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;

use block2::RcBlock;
use codepp_editor::EditorHandle;
use codepp_scintilla_sys::{
    sptr_t, uptr_t, CARETSTYLE_INS_MASK, CARETSTYLE_INVISIBLE, CARETSTYLE_LINE,
    SCI_GETBIDIRECTIONAL, SCI_GETCARETPERIOD, SCI_GETCARETSTYLE, SCI_GETCARETWIDTH,
    SCI_GETCURRENTPOS, SCI_GETDIRECTPOINTER, SCI_GETELEMENTCOLOUR, SCI_GETFOCUS,
    SCI_GETMAINSELECTION, SCI_GETMARGINLEFT, SCI_GETOVERTYPE, SCI_GETSELECTIONHIDDEN,
    SCI_GETSELECTIONNCARETVIRTUALSPACE, SCI_GETSELECTIONS, SCI_GETXOFFSET, SCI_LINEFROMPOSITION,
    SCI_POINTYFROMPOSITION, SCI_SETCARETPERIOD, SCI_SETCARETSTYLE, SCI_TEXTHEIGHT,
    SC_ELEMENT_CARET,
};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_app_kit::{NSColor, NSEvent, NSModalPanelRunLoopMode, NSView};
use objc2_foundation::{
    ns_string, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSRunLoop, NSSize, NSTimer,
};

/// How often the timer re-checks who draws the caret while the caret does
/// not blink, which is what notices a mouse button let go then.
const RECHECK_MS: isize = 500;

thread_local! {
    /// The main view's layer caret, from [`install`] on.
    static CARET: RefCell<Option<Caret>> = const { RefCell::new(None) };
}

/// The layer caret of the main editor view.
struct Caret {
    /// The main view's direct-call handle.
    editor: EditorHandle,
    /// The main view's `SCI_GETDIRECTPOINTER`, for
    /// `scintilla_cocoa_location_x`.
    direct: *mut c_void,
    /// The `ScintillaView*` a plugin addresses, for [`intercept`].
    view: *mut c_void,
    /// The view's `SCIContentView`, whose layer holds [`Caret::layer`].
    content: Retained<NSView>,
    /// The caret itself.
    layer: Retained<AnyObject>,
    /// The blink and re-check timer, and the interval it runs at.
    timer: Option<(Retained<NSTimer>, isize)>,
    /// The caret style Scintilla would have without the layer caret.
    style: usize,
    /// The blink period Scintilla would have without the layer caret.
    period: isize,
    /// Whether Code++ draws the caret now (Scintilla's is invisible).
    owned: bool,
    /// The blink phase: shown when true.
    on: bool,
    /// What the layer was last set to.
    shown: Option<(NSRect, sptr_t, bool)>,
}

/// What decides whether a layer can stand in for Scintilla's caret.
// One flag per condition Scintilla's caret drawing checks.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default)]
struct Facts {
    /// The caret style as asked for (`SCI_SETCARETSTYLE`).
    style: usize,
    /// The caret width; 0 hides the caret (`ViewStyle::IsCaretVisible`).
    width: sptr_t,
    /// How many selections there are.
    selections: sptr_t,
    /// Overtype mode is on.
    overtype: bool,
    /// The main caret's virtual space.
    virtual_space: sptr_t,
    /// Bidirectional layout is on.
    bidirectional: bool,
    /// `SCI_HIDESELECTION` hid the selection, and the caret with it.
    selection_hidden: bool,
    /// An input method is composing.
    composing: bool,
    /// A mouse button is down, as in a drag, whose drop caret is
    /// Scintilla's to draw.
    mouse_down: bool,
}

/// Whether a one-colour rectangle draws exactly what Scintilla would.
fn layer_can_draw(f: &Facts) -> bool {
    f.style & CARETSTYLE_INS_MASK == CARETSTYLE_LINE
        && f.width > 0
        && f.selections == 1
        && !f.overtype
        && f.virtual_space == 0
        && !f.bidirectional
        && !f.selection_hidden
        && !f.composing
        && !f.mouse_down
}

/// The caret style Scintilla is given while the layer stands in: the
/// insert shape invisible, every flag kept.
fn owned_style(style: usize) -> usize {
    (style & !CARETSTYLE_INS_MASK) | CARETSTYLE_INVISIBLE
}

/// The left edge Scintilla gives a line caret whose float client x is
/// `x` (`EditView::DrawCarets`): `round(x - 0.51)`, so a one-point caret
/// straddles the boundary between two characters, except where `x` is
/// the text's own start `text_start` (`SCI_GETMARGINLEFT` less the
/// horizontal scroll) or left of it. A continuation line's wrap indent
/// counts as past the start, as it does in Scintilla.
fn caret_left(x: f64, text_start: f64) -> f64 {
    if x > text_start {
        (x - 0.51).round()
    } else {
        x.round()
    }
}

/// The layer's frame in `SCIContentView` coordinates: Scintilla's client
/// coordinates are relative to the visible part of the content view.
fn caret_frame(
    left: f64,
    y: sptr_t,
    line_height: sptr_t,
    width: sptr_t,
    visible: NSPoint,
) -> NSRect {
    #[allow(clippy::cast_precision_loss)] // pixel coordinates, far below 2^52
    let f = |v: sptr_t| v as f64;
    NSRect::new(
        NSPoint::new(visible.x + left, visible.y + f(y)),
        NSSize::new(f(width.max(1)), f(line_height)),
    )
}

/// Where the layer caret can go, now.
enum Placement {
    /// At this frame, in this colour.
    Draw(NSRect, sptr_t),
    /// Nowhere Scintilla would draw a caret either: the view does not
    /// have the focus, or the caret is scrolled left out of the view.
    Hidden,
    /// Code++ cannot place it: the content view has no layer yet, or
    /// Scintilla's x could not be read. Scintilla keeps the caret.
    Unavailable,
}

/// An `0xAABBGGRR` colour as sRGB components and alpha.
fn rgba(colour: sptr_t) -> (f64, f64, f64, f64) {
    let c = |shift: u32| f64::from(((colour >> shift) & 0xFF) as u8) / 255.0;
    (c(0), c(8), c(16), c(24))
}

/// Set up the layer caret for the main view. Called once, from `run`,
/// before the window is shown; the layer is attached when the content
/// view has its layer.
pub(crate) fn install(editor: EditorHandle, sci_view: &NSView) {
    // SAFETY: `sci_view` is a `ScintillaView`, whose `-content` returns
    // its `SCIContentView` (`cocoa/ScintillaView.h:97`).
    let content: Option<Retained<NSView>> = unsafe { msg_send![sci_view, content] };
    let Some(content) = content else {
        tracing::warn!("no SCIContentView; Scintilla keeps drawing the caret");
        return;
    };
    // SAFETY: `CALayer` is QuartzCore's, linked with AppKit; `+layer`
    // returns an autoreleased new layer, which `Retained` retains.
    let layer: Retained<AnyObject> = unsafe { msg_send![class!(CALayer), layer] };
    // SAFETY: plain `CALayer` property setters.
    unsafe {
        let () = msg_send![&*layer, setName: ns_string!("codepp-caret")];
        let () = msg_send![&*layer, setHidden: true];
        let () = msg_send![&*layer, setZPosition: 10.0_f64];
    }
    let direct = editor.send(SCI_GETDIRECTPOINTER, 0, 0) as *mut c_void;
    let caret = Caret {
        editor,
        direct,
        view: std::ptr::from_ref(sci_view).cast_mut().cast(),
        content,
        layer,
        timer: None,
        style: usize::try_from(editor.send(SCI_GETCARETSTYLE, 0, 0)).unwrap_or(CARETSTYLE_LINE),
        period: editor.send(SCI_GETCARETPERIOD, 0, 0),
        owned: false,
        on: true,
        shown: None,
    };
    CARET.with(|c| *c.borrow_mut() = Some(caret));
    sync();
}

/// Decide again who draws the caret, and place the layer. Called from
/// the main view's notifications. Does nothing before [`install`] or
/// when re-entered.
pub(crate) fn sync() {
    with_caret(Caret::sync);
}

/// The view gained the focus: show the caret at once, as Scintilla does.
pub(crate) fn focus_in() {
    with_caret(|c| {
        c.restart_blink();
        c.sync();
    });
}

/// Answer a plugin's caret style or period message to the main view,
/// so it sets and reads what it would without the layer caret. `None`
/// for any other message or view, which then goes to Scintilla.
pub(crate) fn intercept(view: *mut c_void, msg: u32, wparam: uptr_t) -> Option<sptr_t> {
    if !matches!(
        msg,
        SCI_SETCARETSTYLE | SCI_GETCARETSTYLE | SCI_SETCARETPERIOD | SCI_GETCARETPERIOD
    ) {
        return None;
    }
    CARET.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        let c = guard.as_mut()?;
        if c.view != view {
            return None;
        }
        Some(match msg {
            #[allow(clippy::cast_possible_wrap)] // a small style value
            SCI_GETCARETSTYLE => c.style as sptr_t,
            SCI_GETCARETPERIOD => c.period,
            _ => {
                // Give Scintilla its own values back, let it take the
                // new one as it would (it validates the style), adopt
                // what it kept, and decide again.
                c.release();
                c.editor.send(msg, wparam, 0);
                c.adopt();
                c.restart_blink();
                c.sync();
                0
            }
        })
    })
}

/// Run `f` on the caret, unless there is none or it is in use. A sync
/// skipped that way is made by the next notification or blink tick.
fn with_caret(f: impl FnOnce(&mut Caret)) {
    CARET.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            if let Some(c) = guard.as_mut() {
                f(c);
            }
        }
    });
}

/// A blink tick: flip the phase and decide again.
fn tick() {
    with_caret(|c| {
        if c.owned && c.period > 0 {
            c.on = !c.on;
        }
        c.sync();
    });
}

impl Caret {
    /// Read Scintilla's style and period into [`Caret::style`] and
    /// [`Caret::period`]. While Code++ draws the caret, a value other
    /// than the one Code++ gave Scintilla was set through a direct-call
    /// pointer: it is taken, and the caret handed back to Scintilla with
    /// its own style and period, for [`Caret::sync`] to take again if it
    /// should.
    fn adopt(&mut self) {
        let style =
            usize::try_from(self.editor.send(SCI_GETCARETSTYLE, 0, 0)).unwrap_or(CARETSTYLE_LINE);
        let period = self.editor.send(SCI_GETCARETPERIOD, 0, 0);
        if !self.owned {
            self.style = style;
            self.period = period;
            return;
        }
        let style_changed = style != owned_style(self.style);
        if style_changed {
            self.style = style;
        }
        if period != 0 {
            self.period = period;
        }
        if style_changed || period != 0 {
            // Scintilla still holds Code++'s value for whichever of the
            // two did not change: give it its own for both.
            self.release();
        }
    }

    /// Hand the caret back to Scintilla, with its own style and period.
    fn release(&mut self) {
        if self.owned {
            self.editor.send(SCI_SETCARETSTYLE, self.style, 0);
            self.editor.send(
                SCI_SETCARETPERIOD,
                uptr_t::try_from(self.period).unwrap_or(0),
                0,
            );
            self.owned = false;
        }
    }

    /// Take the caret from Scintilla: invisible, steady.
    fn take(&mut self) {
        if !self.owned {
            self.editor.send(SCI_SETCARETPERIOD, 0, 0);
            self.editor
                .send(SCI_SETCARETSTYLE, owned_style(self.style), 0);
            self.owned = true;
        }
    }

    /// The facts [`layer_can_draw`] decides on, read now.
    fn facts(&self) -> Facts {
        let e = self.editor;
        let main = uptr_t::try_from(e.send(SCI_GETMAINSELECTION, 0, 0)).unwrap_or(0);
        // SAFETY: `SCIContentView` conforms to `NSTextInputClient`.
        let composing: bool = unsafe { msg_send![&*self.content, hasMarkedText] };
        Facts {
            style: self.style,
            width: e.send(SCI_GETCARETWIDTH, 0, 0),
            selections: e.send(SCI_GETSELECTIONS, 0, 0),
            overtype: e.send(SCI_GETOVERTYPE, 0, 0) != 0,
            virtual_space: e.send(SCI_GETSELECTIONNCARETVIRTUALSPACE, main, 0),
            bidirectional: e.send(SCI_GETBIDIRECTIONAL, 0, 0) != 0,
            selection_hidden: e.send(SCI_GETSELECTIONHIDDEN, 0, 0) != 0,
            composing,
            mouse_down: NSEvent::pressedMouseButtons() != 0,
        }
    }

    /// Show the caret and start its blink over, as Scintilla does when
    /// the caret moves.
    fn restart_blink(&mut self) {
        self.on = true;
        if let Some((timer, interval)) = &self.timer {
            #[allow(clippy::cast_precision_loss)] // milliseconds
            let next = NSDate::dateWithTimeIntervalSinceNow(*interval as f64 / 1000.0);
            timer.setFireDate(&next);
        }
    }

    /// Run the timer at the blink period, or at [`RECHECK_MS`] while the
    /// caret does not blink, which still notices a mouse button let go.
    fn ensure_timer(&mut self) {
        let interval = if self.period > 0 {
            self.period
        } else {
            RECHECK_MS
        };
        if matches!(&self.timer, Some((_, i)) if *i == interval) {
            return;
        }
        if let Some((old, _)) = self.timer.take() {
            old.invalidate();
        }
        let block = RcBlock::new(|_timer: NonNull<NSTimer>| {
            crate::at_callback_boundary("caret:tick", (), tick);
        });
        #[allow(clippy::cast_precision_loss)] // milliseconds
        let seconds = interval as f64 / 1000.0;
        // SAFETY: on the main thread; the block touches only this
        // module's main-thread state. Added in the modes Scintilla's own
        // blink timer uses (`ScintillaCocoa::FineTickerStart`), so the
        // caret blinks where it did.
        let timer = unsafe {
            let timer = NSTimer::timerWithTimeInterval_repeats_block(seconds, true, &block);
            let run_loop = NSRunLoop::currentRunLoop();
            run_loop.addTimer_forMode(&timer, NSDefaultRunLoopMode);
            run_loop.addTimer_forMode(&timer, NSModalPanelRunLoopMode);
            timer
        };
        self.timer = Some((timer, interval));
    }

    /// Decide who draws the caret and place the layer.
    fn sync(&mut self) {
        self.adopt();
        // Placed first, so the caret is taken only when the layer can show
        // it: taken and then not placed would leave no caret drawn, and
        // handed back on every failed placement would restyle Scintilla,
        // and repaint, on every paint.
        let place = self.placement();
        let own = !matches!(place, Placement::Unavailable) && layer_can_draw(&self.facts());
        if own != self.owned {
            if own {
                self.take();
            } else {
                self.release();
            }
            self.restart_blink();
        }
        self.ensure_timer();
        let target = match place {
            Placement::Draw(rect, colour) if self.owned => {
                // The caret moved: show it at once, as Scintilla does.
                if self.shown.is_some_and(|(shown, _, _)| shown != rect) {
                    self.restart_blink();
                }
                Some((rect, colour, self.on || self.period <= 0))
            }
            // Scintilla draws, or shows no caret: hide the layer where it is.
            _ => self.shown.map(|(rect, colour, _)| (rect, colour, false)),
        };
        if let Some((rect, colour, visible)) = target {
            if self.shown != target {
                self.apply(rect, colour, visible);
                self.shown = target;
            }
        }
    }

    /// Where the layer caret goes and its colour; see [`Placement`].
    fn placement(&self) -> Placement {
        let e = self.editor;
        if !self.attach() {
            return Placement::Unavailable;
        }
        let pos = e.send(SCI_GETCURRENTPOS, 0, 0);
        let mut x = 0.0;
        // SAFETY: `self.direct` is the main view's direct pointer, the
        // view's `ScintillaCocoa` (`ScintillaCocoa::WndProc`'s
        // `GetDirectPointer`), live for the process since the view is
        // never released; this is the main thread.
        if unsafe { codepp_scintilla_sys::scintilla_cocoa_location_x(self.direct, pos, &raw mut x) }
            == 0
        {
            return Placement::Unavailable;
        }
        let (Ok(upos), true) = (uptr_t::try_from(pos), e.send(SCI_GETFOCUS, 0, 0) != 0) else {
            return Placement::Hidden;
        };
        #[allow(clippy::cast_precision_loss)] // points, far below 2^52
        let text_start = (e.send(SCI_GETMARGINLEFT, 0, 0) - e.send(SCI_GETXOFFSET, 0, 0)) as f64;
        let left = caret_left(x, text_start);
        if left < 0.0 {
            // Scrolled left out of the view; the layer would sit over the
            // margin, outside the content view's clip.
            return Placement::Hidden;
        }
        let y = e.send(SCI_POINTYFROMPOSITION, 0, pos);
        let line = e.send(SCI_LINEFROMPOSITION, upos, 0);
        let height = e.send(SCI_TEXTHEIGHT, uptr_t::try_from(line).unwrap_or(0), 0);
        let width = e.send(SCI_GETCARETWIDTH, 0, 0);
        let colour = e.send(SCI_GETELEMENTCOLOUR, SC_ELEMENT_CARET, 0);
        let visible = self.content.visibleRect().origin;
        Placement::Draw(caret_frame(left, y, height, width, visible), colour)
    }

    /// Put the layer on the content view's layer, again if that layer
    /// was replaced. False while the content view has none.
    fn attach(&self) -> bool {
        // SAFETY: `-layer` and `-superlayer` are plain getters.
        let host: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.content, layer] };
        let Some(host) = host else {
            return false;
        };
        // SAFETY: as above.
        let parent: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.layer, superlayer] };
        if parent.is_none_or(|p| Retained::as_ptr(&p) != Retained::as_ptr(&host)) {
            // SAFETY: `host` is a live `CALayer`, and the caret layer
            // is one too.
            // AppKit owns the backing layer and could drop sublayers it did
            // not add; this check, made at every sync, puts the caret back.
            unsafe {
                let () = msg_send![&*host, addSublayer: &*self.layer];
            }
        }
        true
    }

    /// Set the layer's frame, colour and visibility, with no animation.
    fn apply(&self, rect: NSRect, colour: sptr_t, visible: bool) {
        let (r, g, b, a) = rgba(colour);
        let cg = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a).CGColor();
        // SAFETY: `CATransaction` class methods and `CALayer` setters,
        // on the main thread. Actions are disabled so a blink or a move
        // is not animated, as Scintilla's caret is not.
        unsafe {
            let () = msg_send![class!(CATransaction), begin];
            let () = msg_send![class!(CATransaction), setDisableActions: true];
            let () = msg_send![&*self.layer, setFrame: rect];
            let () = msg_send![&*self.layer, setBackgroundColor: &*cg];
            let () = msg_send![&*self.layer, setHidden: !visible];
            let () = msg_send![class!(CATransaction), commit];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usual() -> Facts {
        Facts {
            style: CARETSTYLE_LINE,
            width: 1,
            selections: 1,
            ..Facts::default()
        }
    }

    /// The layer stands in only for a single line caret in insert mode,
    /// and hands every other case back to Scintilla.
    #[test]
    fn the_layer_draws_only_what_a_rectangle_can() {
        assert!(layer_can_draw(&usual()));
        // Flags above the insert shape do not matter to a line caret.
        assert!(layer_can_draw(&Facts {
            style: CARETSTYLE_LINE | 0x10,
            ..usual()
        }));
        for f in [
            Facts {
                style: CARETSTYLE_INVISIBLE,
                ..usual()
            },
            Facts {
                style: 2,
                ..usual()
            }, // CARETSTYLE_BLOCK
            Facts {
                width: 0,
                ..usual()
            }, // SCI_SETCARETWIDTH(0) hides the caret
            Facts {
                selections: 2,
                ..usual()
            },
            Facts {
                overtype: true,
                ..usual()
            },
            Facts {
                virtual_space: 1,
                ..usual()
            },
            Facts {
                bidirectional: true,
                ..usual()
            },
            Facts {
                selection_hidden: true,
                ..usual()
            },
            Facts {
                composing: true,
                ..usual()
            },
            Facts {
                mouse_down: true,
                ..usual()
            },
        ] {
            assert!(!layer_can_draw(&f), "{f:?}");
        }
    }

    /// Scintilla's caret is made invisible by its insert shape alone, so
    /// overtype and block-after flags survive the hand-over.
    #[test]
    fn the_owned_style_keeps_the_flags() {
        assert_eq!(owned_style(CARETSTYLE_LINE), CARETSTYLE_INVISIBLE);
        assert_eq!(owned_style(CARETSTYLE_LINE | 0x110), 0x110);
    }

    /// A caret inside a line straddles the boundary, `round(x - 0.51)`,
    /// so a whole-point x moves one point left and a fractional one rounds
    /// down; at the text's start it is not moved. The x values
    /// are the ones measured at Courier New 10 and Helvetica Neue 13.
    #[test]
    fn the_left_edge_is_scintillas() {
        assert!((caret_left(1.0, 1.0) - 1.0).abs() < f64::EPSILON);
        assert!((caret_left(7.0005, 1.0) - 6.0).abs() < f64::EPSILON);
        assert!((caret_left(7.6, 1.0) - 7.0).abs() < f64::EPSILON);
        assert!((caret_left(67.995, 1.0) - 67.0).abs() < f64::EPSILON);
        // A continuation line's wrap indent is past the start.
        assert!((caret_left(25.0, 1.0) - 24.0).abs() < f64::EPSILON);
        // Scrolled so the line's start is left of the view.
        assert!((caret_left(-40.0, -49.0) - -41.0).abs() < f64::EPSILON);
    }

    /// The frame is the left edge offset by the visible rect, the line's
    /// height, the caret width at least one point.
    #[test]
    fn the_frame_follows_the_visible_rect() {
        let r = caret_frame(60.0, 33, 11, 1, NSPoint::new(0.0, 0.0));
        assert_eq!(
            r,
            NSRect::new(NSPoint::new(60.0, 33.0), NSSize::new(1.0, 11.0))
        );
        let r = caret_frame(60.0, 33, 11, 0, NSPoint::new(5.0, 300.0));
        assert_eq!(
            r,
            NSRect::new(NSPoint::new(65.0, 333.0), NSSize::new(1.0, 11.0))
        );
    }

    /// `0xAABBGGRR` is read with red in the low byte and alpha in the high.
    #[test]
    fn colours_are_read_red_first() {
        assert_eq!(rgba(0xFF00_00FF), (1.0, 0.0, 0.0, 1.0));
        assert_eq!(rgba(0x00FF_0000), (0.0, 0.0, 1.0, 0.0));
    }

    /// The code of `src` before its first test module, with comment lines
    /// dropped and whitespace removed, so the guards below match calls and
    /// not prose or layout.
    fn code(src: &str) -> String {
        let src = src.find("#[cfg(test)]").map_or(src, |i| &src[..i]);
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<String>()
            .split_whitespace()
            .collect()
    }

    /// A plugin's `SCI_*` to a view reaches Scintilla only after the
    /// caret has been asked, on the same-thread path and in the hop from
    /// another thread, so a plugin cannot set or read the caret style or
    /// period around the layer caret. Nothing at run time shows a call
    /// that skips it: Scintilla answers either way.
    #[test]
    fn plugin_messages_ask_the_caret_first() {
        let plugin = code(include_str!("plugin.rs"));
        let sends = plugin.matches("scintilla_cocoa_send_message(").count();
        let asked = plugin
            .match_indices("crate::caret::intercept(")
            .filter(|(i, _)| {
                plugin[*i..]
                    .find("scintilla_cocoa_send_message(")
                    .is_some_and(|d| d < 160)
            })
            .count();
        assert_eq!(
            sends, 2,
            "plugin.rs sends to Scintilla in {sends} places, where two were guarded"
        );
        assert_eq!(
            asked, 2,
            "a send to Scintilla in plugin.rs does not ask the caret first"
        );
    }

    /// The caret is installed with the view, and synced from the main
    /// view's notifications: a caret move or scroll (`SCN_UPDATEUI`), a
    /// paint (a resize, a zoom, a margin change), and focus in and out.
    /// A missing call leaves the layer where it was, which no test sees.
    #[test]
    fn the_caret_hears_the_main_views_notifications() {
        let lib = code(include_str!("lib.rs"));
        let notify = lib
            .find("unsafefnon_sci_notify_inner(")
            .expect("on_sci_notify_inner");
        let body = &lib[notify..];
        // The arm's own block, brace-matched, so a call in a neighbouring
        // arm does not count.
        let arm = |code: &str| {
            let pat = format!("codepp_scintilla_sys::{code}=>{{");
            let at = body.find(&pat).unwrap_or_else(|| panic!("no {code} arm")) + pat.len();
            let mut depth = 1;
            let end = body[at..]
                .char_indices()
                .find(|&(_, c)| {
                    depth += match c {
                        '{' => 1,
                        '}' => -1,
                        _ => 0,
                    };
                    depth == 0
                })
                .map(|(i, _)| at + i)
                .expect("the arm closes");
            body[at..end].contains("caret::sync();")
        };
        assert!(body.contains("codepp_scintilla_sys::SCN_FOCUSIN=>caret::focus_in(),"));
        assert!(body.contains("codepp_scintilla_sys::SCN_FOCUSOUT=>caret::sync(),"));
        assert!(body.contains("ifcode==codepp_scintilla_sys::SCN_UPDATEUI{caret::sync();}"));
        assert!(
            arm("SCN_PAINTED"),
            "the SCN_PAINTED arm does not sync the caret"
        );
        let callback = lib
            .find("scintilla_cocoa_set_notify_callback(sci_ptr,on_sci_notify,0);")
            .expect("callback");
        let install = lib
            .find("caret::install(editor,&sci_view);")
            .expect("caret::install in run");
        assert!(
            install > callback,
            "the caret is installed before the view notifies"
        );
    }
}
