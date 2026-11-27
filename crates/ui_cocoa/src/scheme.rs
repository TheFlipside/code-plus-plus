//! The colour scheme on macOS: light or dark, as the application's
//! effective appearance is (DESIGN.md §7.4, the dark-mode entry).
//!
//! The user chooses in Preferences → Dark Mode: light, dark, or follow
//! the system ([`Appearance`]). The choice is applied as
//! `NSApp.appearance` ([`apply_appearance`]): Aqua, Dark Aqua, or none,
//! which follows System Settings. That makes AppKit draw every window,
//! panel, alert and menu of the app in it, and the editor follows the same
//! appearance, so there is one answer to "is Code++ dark?": the one AppKit
//! gives ([`scheme_of`]).
//!
//! It is read once at startup, before anything is styled ([`init`]), and
//! again whenever the window's content view is told its effective
//! appearance changed (`crate::dropview`), which AppKit does both for a
//! change in System Settings and for a change of `NSApp.appearance`. The
//! check runs in the main run loop's default mode ([`queue_check`]): the
//! notification can arrive inside a borrow of the window's state, and
//! the change restyles the editor through that state.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;

use block2::{RcBlock, StackBlock};
use codepp_core::colour::{mix, pack, to_colorref, ColourScheme};
use codepp_core::preferences::Appearance;
use codepp_plugin_host::NppDarkModeColors;
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication, NSColor,
    NSColorSpace,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSDefaultRunLoopMode, NSRunLoop, NSTimer};

use crate::state::with_state;

/// What the appearance said the last time it was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reading {
    scheme: ColourScheme,
    /// The window background, `0xRRGGBB`.
    background: u32,
    /// The appearance's colours as `NPPM_GETDARKMODECOLORS` hands them out.
    colours: NppDarkModeColors,
}

thread_local! {
    /// The last reading, for what is drawn without the window's state:
    /// the tab strip's tints, print, the plugins' colours.
    static READING: RefCell<Option<Reading>> = const { RefCell::new(None) };
    /// Set while a check is queued, so a burst of notifications costs one.
    static CHECK_QUEUED: Cell<bool> = const { Cell::new(false) };
}

/// Apply the user's choice as `NSApp.appearance`. Every window follows
/// it, and the content view's `viewDidChangeEffectiveAppearance` queues
/// the check that restyles the editor.
pub(crate) fn apply_appearance(appearance: Appearance, mtm: MainThreadMarker) {
    // SAFETY: extern statics AppKit initialises before any code of ours
    // runs, read on the main thread.
    let name = match appearance {
        Appearance::Light => Some(unsafe { NSAppearanceNameAqua }),
        Appearance::Dark => Some(unsafe { NSAppearanceNameDarkAqua }),
        Appearance::FollowSystem => None,
    };
    let chosen = name.and_then(NSAppearance::appearanceNamed);
    NSApplication::sharedApplication(mtm).setAppearance(chosen.as_deref());
    tracing::info!(?appearance, "appearance applied");
}

/// The scheme AppKit's effective appearance `best` stands for: dark for
/// Dark Aqua and its relatives, light for anything else.
fn scheme_of(appearance: &NSAppearance) -> ColourScheme {
    // SAFETY: as in `apply_appearance`.
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names = NSArray::from_slice(&[aqua, dark]);
    match appearance.bestMatchFromAppearancesWithNames(&names) {
        Some(best) if &*best == dark => ColourScheme::Dark,
        _ => ColourScheme::Light,
    }
}

/// `colour` as `0xRRGGBB` in sRGB, laid over `over` by its alpha, or
/// `None` for a colour that has no sRGB form (a pattern, a catalog colour
/// with no component values). Converted before its components are read:
/// `redComponent` raises an Objective-C exception for a colour in another
/// space, which crossing the FFI boundary is undefined behaviour.
fn srgb(colour: &NSColor, over: Option<u32>) -> Option<u32> {
    let c = colour.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    let byte = |v: f64| {
        let scaled = (v.clamp(0.0, 1.0) * 255.0).round();
        // In 0..=255 after the clamp.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let b = scaled as u8;
        b
    };
    let rgb = pack(
        byte(c.redComponent()),
        byte(c.greenComponent()),
        byte(c.blueComponent()),
    );
    let alpha = c.alphaComponent().clamp(0.0, 1.0);
    Some(match over {
        // A percentage in 0..=100 after the clamp.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(bg) if alpha < 1.0 => mix(rgb, bg, (alpha * 100.0).round() as u32),
        _ => rgb,
    })
}

/// Read the scheme and the colours from the application's effective
/// appearance. The semantic colours are resolved under that appearance,
/// which `performAsCurrentDrawingAppearance` makes the current one for
/// the block.
fn read(mtm: MainThreadMarker) -> Reading {
    let appearance = NSApplication::sharedApplication(mtm).effectiveAppearance();
    let scheme = scheme_of(&appearance);
    let out = Cell::new(None);
    // A stack block may borrow `out` because the call below is
    // synchronous: AppKit runs the block before it returns and keeps no
    // copy of it, so nothing can call it once this frame is gone.
    let block = StackBlock::new(|| out.set(Some(resolve_colours())));
    appearance.performAsCurrentDrawingAppearance(&block);
    let (background, colours) = out.take().unwrap_or_else(|| {
        let own = NppDarkModeColors::codepp();
        (to_colorref(own.dlg_background), own)
    });
    Reading {
        scheme,
        background,
        colours,
    }
}

/// The window background (`0xRRGGBB`) and the colours a plugin is given,
/// from the current drawing appearance's semantic colours, with Code++'s
/// own ([`NppDarkModeColors::codepp`]) for any that has no sRGB form.
fn resolve_colours() -> (u32, NppDarkModeColors) {
    let own = NppDarkModeColors::codepp();
    // `own` holds COLORREFs; `to_colorref` is its own inverse (it swaps
    // the red and blue bytes), so it gives back `0xRRGGBB` too.
    let window = srgb(&NSColor::windowBackgroundColor(), None)
        .unwrap_or_else(|| to_colorref(own.dlg_background));
    let get = |c: &NSColor, fallback: u32| srgb(c, Some(window)).unwrap_or(to_colorref(fallback));
    let text = get(&NSColor::labelColor(), own.text);
    let colours = NppDarkModeColors {
        background: to_colorref(get(&NSColor::textBackgroundColor(), own.background)),
        ctrl_background: to_colorref(get(&NSColor::controlBackgroundColor(), own.ctrl_background)),
        hot_background: to_colorref(mix(text, window, 12)),
        dlg_background: to_colorref(window),
        error_background: to_colorref(get(&NSColor::systemRedColor(), own.error_background)),
        text: to_colorref(text),
        darker_text: to_colorref(get(&NSColor::secondaryLabelColor(), own.darker_text)),
        disabled_text: to_colorref(get(&NSColor::tertiaryLabelColor(), own.disabled_text)),
        link_text: to_colorref(get(&NSColor::linkColor(), own.link_text)),
        edge: to_colorref(get(&NSColor::separatorColor(), own.edge)),
        hot_edge: to_colorref(mix(text, window, 60)),
        disabled_edge: to_colorref(get(&NSColor::quaternaryLabelColor(), own.disabled_edge)),
    };
    (window, colours)
}

/// Apply the user's choice and read the scheme at startup, and record it
/// in the shell before anything is styled. Call once the window's state
/// is installed and before the first styling.
pub(crate) fn init(mtm: MainThreadMarker) {
    if let Some(appearance) = with_state(|st| st.shell.preferences.dark_mode.appearance) {
        apply_appearance(appearance, mtm);
    }
    let reading = read(mtm);
    READING.with(|r| *r.borrow_mut() = Some(reading));
    with_state(|st| st.shell.set_initial_colour_scheme(reading.scheme));
    tracing::info!(scheme = reading.scheme.name(), "colour scheme at startup");
}

/// The effective appearance may have changed: queue one check for the
/// main run loop's default mode. Never restyles here, since this can run
/// under a state borrow.
pub(crate) fn queue_check() {
    schedule(0.0);
}

/// How long a check whose state read was declined waits before it tries
/// again, in seconds. Not at once: the read is declined while a nested
/// run loop runs below a state borrow, and a check queued for the next
/// turn of such a loop would be run by it at once, every turn.
const RETRY_AFTER_SECS: f64 = 0.25;

/// Queue the check after `seconds`, in the default run-loop mode only, so
/// it runs neither inside a modal dialog nor while a gesture is tracked.
fn schedule(seconds: f64) {
    if CHECK_QUEUED.with(|q| q.replace(true)) {
        return;
    }
    let block = RcBlock::new(|_timer: NonNull<NSTimer>| {
        crate::at_callback_boundary("scheme:check", (), check);
    });
    // SAFETY: on the main thread, where every caller is; the block
    // touches only this module's main-thread state and `with_state`. The
    // extern static is AppKit's, initialised before any code of ours runs.
    unsafe {
        let timer = NSTimer::timerWithTimeInterval_repeats_block(seconds, false, &block);
        NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSDefaultRunLoopMode);
    }
}

/// Read the appearance again and, if its scheme changed, tell the shell,
/// which restyles the editor and queues `NPPN_DARKMODECHANGED`; then
/// drain, so plugins hear of it and the tab strip is repainted in the new
/// tints.
///
/// The reading is recorded only once the shell has been told, so what is
/// drawn without the state (the tints, print, the plugins' colours) never
/// says another scheme than the editor. Where the state cannot be reached
/// the check is tried again ([`RETRY_AFTER_SECS`]), since nothing tells
/// this module twice about the same change.
fn check() {
    CHECK_QUEUED.with(|q| q.set(false));
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if crate::quitting() {
        return;
    }
    let reading = read(mtm);
    let told = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.set_colour_scheme(&mut ui, reading.scheme)
    });
    let Some(changed) = told else {
        // Uncapped, but a turn of the run loop apart: it ends when the
        // borrow that declined the read is let go.
        schedule(RETRY_AFTER_SECS);
        return;
    };
    let previous = READING.with(|r| r.borrow_mut().replace(reading));
    if changed {
        crate::drain_shell();
    } else if previous.map(|p| p.background) != Some(reading.background) {
        // The same scheme with another background (Increase Contrast, say):
        // only the tints moved.
        crate::refresh_tab_chrome();
    }
}

/// The scheme as last read; light before the first reading.
#[must_use]
pub(crate) fn current() -> ColourScheme {
    READING.with(|r| r.borrow().map_or(ColourScheme::Light, |r| r.scheme))
}

/// The background an inactive tab's tint is laid over in the dark scheme:
/// the window background. `None` in the light scheme, where the tint is
/// laid over white as it always was (`TabColor::tint_rgb`).
#[must_use]
pub(crate) fn tint_base() -> Option<u32> {
    READING.with(|r| {
        r.borrow()
            .filter(|r| r.scheme == ColourScheme::Dark)
            .map(|r| r.background)
    })
}

/// The appearance's colours for `NPPM_GETDARKMODECOLORS`, while the
/// scheme is dark; `None` otherwise, when the shell answers with Code++'s
/// own.
#[must_use]
pub(crate) fn dark_colours() -> Option<NppDarkModeColors> {
    READING.with(|r| {
        r.borrow()
            .filter(|r| r.scheme == ColourScheme::Dark)
            .map(|r| r.colours)
    })
}

/// The scenario behind `smoke_support::the_scheme_follows_the_appearance`,
/// for `tests/cocoa_smoke.rs`, which alone owns the main thread an
/// `NSApplication` needs.
#[cfg(debug_assertions)]
pub(crate) mod smoke {
    use codepp_core::colour::{is_dark, to_colorref, ColourScheme};
    use codepp_core::preferences::Appearance;
    use codepp_plugin_host::NppDarkModeColors;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;

    pub(crate) fn the_scheme_follows_the_appearance() {
        let mtm = MainThreadMarker::new().expect("the smoke binary owns the main thread");
        let app = NSApplication::sharedApplication(mtm);

        super::apply_appearance(Appearance::Dark, mtm);
        let dark = super::read(mtm);
        assert_eq!(dark.scheme, ColourScheme::Dark, "Dark Aqua read as light");
        assert!(
            is_dark(dark.background),
            "the dark window background is not dark: {:06X}",
            dark.background
        );
        assert_eq!(dark.colours.dlg_background, to_colorref(dark.background));
        assert!(
            !is_dark(to_colorref(dark.colours.text)),
            "the dark appearance's text is not light"
        );
        assert_ne!(
            dark.colours,
            NppDarkModeColors::codepp(),
            "the plugin colours are Code++'s own, not the appearance's"
        );

        super::apply_appearance(Appearance::Light, mtm);
        let light = super::read(mtm);
        assert_eq!(light.scheme, ColourScheme::Light, "Aqua read as dark");
        assert!(
            !is_dark(light.background),
            "the light window background is dark: {:06X}",
            light.background
        );

        super::apply_appearance(Appearance::FollowSystem, mtm);
        assert!(
            app.appearance().is_none(),
            "following the system left an appearance of the app's own"
        );
    }
}
