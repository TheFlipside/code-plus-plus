//! The colour scheme on GTK: light or dark, as the user's GTK theme is
//! (DESIGN.md §7.4, the dark-mode entry).
//!
//! GTK 3 offers an application no way to force a light or a dark look
//! under any theme: `gtk-application-prefer-dark-theme` darkens only a
//! theme that ships a dark variant, and nothing lightens a theme that is
//! dark itself. So Code++ on GTK has no setting and follows the theme,
//! which is how the desktop's own choice reaches a GTK 3 application.
//!
//! The scheme is read from the theme's own colours: dark when its text
//! (`theme_fg_color`) is lighter than its window background
//! (`theme_bg_color`). That works for any theme, where the theme's name
//! or the `prefer-dark` setting would say nothing about a theme that is
//! dark in itself (Mint-Y-Dark, Adwaita-dark chosen by name). A colour the
//! theme does not name reads as light.
//!
//! It is read once at startup, before anything is styled
//! ([`init`]), and again whenever the main window's style is updated,
//! which GTK does when the theme or `prefer-dark` changes. That handler
//! only queues a check for the main loop's next idle ([`on_style_updated`]):
//! `style-updated` can fire while the window's state is borrowed, and the
//! change restyles the editor through that state.

use std::cell::{Cell, RefCell};

use codepp_core::colour::{is_dark, mix, relative_luminance, to_colorref, ColourScheme};
use codepp_plugin_host::NppDarkModeColors;
use gtk::glib;
use gtk::prelude::*;

use crate::state::with_state;

/// What the theme said the last time it was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reading {
    scheme: ColourScheme,
    /// The theme's window background, `0xRRGGBB`, if it names one.
    background: Option<u32>,
    /// The theme's colours as `NPPM_GETDARKMODECOLORS` hands them out.
    colours: NppDarkModeColors,
}

thread_local! {
    /// The last reading, for what is drawn without the window's state:
    /// the tab strip's tints, print, the plugins' colours.
    static READING: RefCell<Option<Reading>> = const { RefCell::new(None) };
    /// Set while a check is queued, so a burst of `style-updated` costs
    /// one check.
    static CHECK_QUEUED: Cell<bool> = const { Cell::new(false) };
}

/// The scheme a theme's text and background colours (`0xRRGGBB`) make:
/// dark when the text is lighter than the background. With no text
/// colour, dark when the background is; with no background, light.
#[must_use]
pub(crate) fn scheme_of(fg: Option<u32>, bg: Option<u32>) -> ColourScheme {
    let dark = match (fg, bg) {
        (Some(fg), Some(bg)) => relative_luminance(fg) > relative_luminance(bg),
        (None, Some(bg)) => is_dark(bg),
        (_, None) => false,
    };
    if dark {
        ColourScheme::Dark
    } else {
        ColourScheme::Light
    }
}

/// The colours `NPPM_GETDARKMODECOLORS` hands a plugin, from the theme's
/// named colours (`lookup` answers `0xRRGGBB` for a name, or `None`), with
/// Code++'s own dark colours ([`NppDarkModeColors::codepp`]) for anything
/// the theme does not name. The result is `COLORREF`s.
#[must_use]
pub(crate) fn colours_from(lookup: impl Fn(&str) -> Option<u32>) -> NppDarkModeColors {
    let own = NppDarkModeColors::codepp();
    let rgb = |name: &str, fallback_colorref: u32| {
        lookup(name).unwrap_or_else(|| to_colorref(fallback_colorref))
    };
    let window = rgb("theme_bg_color", own.dlg_background);
    let base = rgb("theme_base_color", own.background);
    let text = rgb("theme_fg_color", own.text);
    let colour = |rgb_value: u32| to_colorref(rgb_value);
    NppDarkModeColors {
        background: colour(base),
        ctrl_background: colour(window),
        hot_background: colour(mix(text, window, 12)),
        dlg_background: colour(window),
        error_background: colour(rgb("error_color", own.error_background)),
        text: colour(text),
        darker_text: colour(mix(text, window, 80)),
        disabled_text: colour(rgb("insensitive_fg_color", own.disabled_text)),
        link_text: colour(rgb("link_color", own.link_text)),
        edge: colour(rgb("borders", own.edge)),
        hot_edge: colour(mix(text, window, 60)),
        disabled_edge: colour(rgb("unfocused_borders", own.disabled_edge)),
    }
}

/// Read the scheme and colours from `widget`'s theme.
fn read(widget: &impl IsA<gtk::Widget>) -> Reading {
    let context = widget.style_context();
    let lookup = |name: &str| {
        context.lookup_color(name).map(|c| {
            let byte = |v: f64| {
                let scaled = (v.clamp(0.0, 1.0) * 255.0).round();
                // In 0..=255 after the clamp.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let b = scaled as u8;
                b
            };
            codepp_core::colour::pack(byte(c.red()), byte(c.green()), byte(c.blue()))
        })
    };
    let background = lookup("theme_bg_color");
    Reading {
        scheme: scheme_of(lookup("theme_fg_color"), background),
        background,
        colours: colours_from(lookup),
    }
}

/// Read the theme at startup, record the scheme in the shell before
/// anything is styled, and follow the theme from here on. Call once the
/// window's state is installed and before the first styling.
pub(crate) fn init(window: &gtk::Window) {
    let reading = read(window);
    READING.with(|r| *r.borrow_mut() = Some(reading));
    with_state(|st| st.shell.set_initial_colour_scheme(reading.scheme));
    tracing::info!(scheme = reading.scheme.name(), "colour scheme at startup");
    window.connect_style_updated(|_| {
        crate::at_callback_boundary("scheme:style-updated", (), on_style_updated);
    });
}

/// The main window's style was updated: queue one check for the next
/// idle. Never restyles here, since this can run under a state borrow.
fn on_style_updated() {
    if CHECK_QUEUED.with(Cell::get) {
        return;
    }
    CHECK_QUEUED.with(|q| q.set(true));
    glib::idle_add_local_once(|| {
        crate::at_callback_boundary("scheme:check", (), check);
    });
}

/// How long a check whose state read was declined waits before it tries
/// again. A timeout rather than an idle: the read is declined while a
/// nested main loop runs below a state borrow, and an idle would be
/// dispatched by that loop at once, every iteration, for as long as it
/// runs.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_millis(250);

/// Read the theme again and, if its scheme changed, tell the shell, which
/// restyles the editor and queues `NPPN_DARKMODECHANGED`; then drain, so
/// plugins hear of it and the tab strip is repainted in the new tints.
///
/// The reading is recorded only once the shell has been told, so what is
/// drawn without the state (the tints, print, the plugins' colours) never
/// says another scheme than the editor. Where the state cannot be reached
/// the check is tried again ([`RETRY_AFTER`]), since `style-updated` will
/// not fire a second time for the same change. A drain held back, by the
/// drain freeze or because the app is quitting, is not retried here: the
/// notification stays queued in the shell, and the freeze's holder drains
/// when it lets go.
fn check() {
    CHECK_QUEUED.with(|q| q.set(false));
    let Some(window) = with_state(|st| st.window.clone()) else {
        retry();
        return;
    };
    let reading = read(&window);
    tracing::debug!(
        scheme = reading.scheme.name(),
        "theme read after a style update"
    );
    let told = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.set_colour_scheme(&mut ui, reading.scheme)
    });
    let Some(changed) = told else {
        retry();
        return;
    };
    let previous = READING.with(|r| r.borrow_mut().replace(reading));
    if changed {
        crate::drain_shell();
    } else if previous.map(|p| p.background) != Some(reading.background) {
        // The same scheme from another theme: only the tints moved.
        crate::refresh_tab_chrome();
    }
}

/// Queue the check again after [`RETRY_AFTER`], unless one is queued.
fn retry() {
    if CHECK_QUEUED.with(Cell::get) || crate::quitting() {
        return;
    }
    CHECK_QUEUED.with(|q| q.set(true));
    glib::timeout_add_local_once(RETRY_AFTER, || {
        crate::at_callback_boundary("scheme:retry", (), check);
    });
}

/// The scheme as last read; light before the first reading.
#[must_use]
pub(crate) fn current() -> ColourScheme {
    READING.with(|r| r.borrow().map_or(ColourScheme::Light, |r| r.scheme))
}

/// The background an inactive tab's tint is laid over in the dark scheme:
/// the theme's window background. `None` in the light scheme, where the
/// tint is laid over white as it always was (`TabColor::tint_rgb`).
#[must_use]
pub(crate) fn tint_base() -> Option<u32> {
    READING.with(|r| {
        r.borrow()
            .filter(|r| r.scheme == ColourScheme::Dark)
            .and_then(|r| r.background)
    })
}

/// The theme's colours for `NPPM_GETDARKMODECOLORS`, while the scheme is
/// dark; `None` otherwise, when the shell answers with Code++'s own.
#[must_use]
pub(crate) fn dark_colours() -> Option<NppDarkModeColors> {
    READING.with(|r| {
        r.borrow()
            .filter(|r| r.scheme == ColourScheme::Dark)
            .map(|r| r.colours)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scheme_is_dark_when_the_text_is_lighter_than_the_background() {
        // Adwaita, light and dark.
        assert_eq!(
            scheme_of(Some(0x2E_34_36), Some(0xF6_F5_F4)),
            ColourScheme::Light
        );
        assert_eq!(
            scheme_of(Some(0xEE_EE_EC), Some(0x35_35_35)),
            ColourScheme::Dark
        );
        // A theme dark in itself, whatever its name.
        assert_eq!(
            scheme_of(Some(0xDA_DA_DA), Some(0x2F_2F_2F)),
            ColourScheme::Dark
        );
        // A grey Windows-95-style theme with black text.
        assert_eq!(scheme_of(Some(0), Some(0xC0_C0_C0)), ColourScheme::Light);
    }

    #[test]
    fn a_colour_the_theme_does_not_name_reads_as_light() {
        assert_eq!(scheme_of(None, None), ColourScheme::Light);
        assert_eq!(scheme_of(Some(0xFF_FF_FF), None), ColourScheme::Light);
        assert_eq!(scheme_of(None, Some(0x20_20_20)), ColourScheme::Dark);
        assert_eq!(scheme_of(None, Some(0xF0_F0_F0)), ColourScheme::Light);
    }

    #[test]
    fn the_plugin_colours_come_from_the_theme_and_code_pp_fills_the_rest() {
        let theme = |name: &str| match name {
            "theme_bg_color" => Some(0x35_35_35),
            "theme_base_color" => Some(0x2D_2D_2D),
            "theme_fg_color" => Some(0xEE_EE_EC),
            "borders" => Some(0x1B_1B_1B),
            _ => None,
        };
        let c = colours_from(theme);
        assert_eq!(c.background, to_colorref(0x2D_2D_2D));
        assert_eq!(c.dlg_background, to_colorref(0x35_35_35));
        assert_eq!(c.ctrl_background, to_colorref(0x35_35_35));
        assert_eq!(c.text, to_colorref(0xEE_EE_EC));
        assert_eq!(c.edge, to_colorref(0x1B_1B_1B));
        let own = NppDarkModeColors::codepp();
        assert_eq!(c.link_text, own.link_text, "not named: Code++'s own");
        assert_eq!(c.error_background, own.error_background);
        // A theme that names nothing gives Code++'s own throughout, but
        // for the mixes, which are of Code++'s own text and background.
        let none = colours_from(|_| None);
        assert_eq!(none.background, own.background);
        assert_eq!(none.text, own.text);
        assert_eq!(none.dlg_background, own.dlg_background);
    }
}

/// Driven by `crate::display_tests`, which says why every GTK scenario in
/// this crate shares one test function.
#[cfg(test)]
pub(crate) mod display_tests {
    use super::*;

    /// The scheme read from a real window's style context follows the
    /// theme: Adwaita, which GTK carries built in, is light, and its dark
    /// variant, asked for through `gtk-application-prefer-dark-theme`, is
    /// dark. The colours handed to plugins are the variant's own.
    pub(crate) fn the_scheme_follows_the_theme() {
        gtk::init().expect("GTK could not be initialised");
        let settings = gtk::Settings::default().expect("no GtkSettings");
        let theme_name = settings.gtk_theme_name();
        let prefer_dark = settings.is_gtk_application_prefer_dark_theme();
        settings.set_gtk_theme_name(Some("Adwaita"));
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        let read_with = |dark: bool| {
            settings.set_gtk_application_prefer_dark_theme(dark);
            while gtk::events_pending() {
                gtk::main_iteration();
            }
            read(&window)
        };
        let light = read_with(false);
        assert_eq!(light.scheme, ColourScheme::Light, "Adwaita read as dark");
        let dark = read_with(true);
        assert_eq!(
            dark.scheme,
            ColourScheme::Dark,
            "Adwaita's dark variant read as light"
        );
        assert!(
            dark.background.is_some_and(is_dark),
            "the dark variant's window background is not dark: {:06X?}",
            dark.background
        );
        assert_ne!(
            dark.colours.text,
            NppDarkModeColors::codepp().text,
            "the plugin colours are Code++'s own, not the theme's"
        );
        assert_eq!(
            read_with(false).scheme,
            ColourScheme::Light,
            "the switch back failed"
        );
        // Leave the settings as the scenarios before this one had them.
        settings.set_gtk_application_prefer_dark_theme(prefer_dark);
        settings.set_gtk_theme_name(theme_name.as_deref());
        // SAFETY: a toplevel this scenario made and nothing else holds.
        unsafe { window.destroy() };
    }
}
