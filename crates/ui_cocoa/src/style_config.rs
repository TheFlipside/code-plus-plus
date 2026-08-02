//! The Style Configurator dialog for the Cocoa backend.
//!
//! Mirrors `ui_gtk::style_config` and the Win32
//! `handle_style_config_menu` / `show_style_config_dialog` pair. It edits
//! the one style scope wired on any backend so far — the **Default
//! Style** ([`codepp_core::styles::StyleEntry`], the `STYLE_DEFAULT`
//! index that `SCI_STYLECLEARALL` propagates to every other style) plus
//! window [`Transparency`].
//!
//! **No live preview**, matching both other backends: the controls
//! accumulate changes and nothing reaches the editor until Save & Close.
//!
//! The theme / language / style popups are parity chrome — "Default
//! (stylers.xml)", "Global Styles", "Default Style" — reflecting that
//! only the default style is editable today, which is what the `Styles`
//! data model holds. `ui_gtk` renders the style scope as a one-row list
//! to match Win32's list box; three popups is the same information in
//! the shape macOS uses for a fixed choice, and it keeps the accessory
//! view short enough to sit in an alert.
//!
//! On Save & Close the controls are read into a fresh `Styles`, then
//! `Shell::set_styles` persists it and `apply_default_style` +
//! `apply_lang` push it to the live editor — the same sequence, and the
//! same reason for the trailing `apply_lang` (re-layer the lexer's
//! per-style colours that `SCI_STYLECLEARALL` just wiped), as the other
//! two backends. That whole data path is already cross-platform.

use objc2::rc::Retained;
use objc2::MainThreadOnly;
use objc2_app_kit::{
    NSAlert, NSButton, NSButtonType, NSColor, NSColorSpace, NSColorWell, NSFontManager,
    NSPopUpButton, NSSlider, NSTextField, NSTextFieldBezelStyle, NSView,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use codepp_core::styles::{format_rgb_hex, parse_rgb_hex, StyleEntry, Styles, Transparency};

use crate::state::with_state;

/// Opacity slider bounds — fully opaque down to a 20% floor, below which
/// the editor is unreadable. Matches the Win32 slider, `ui_gtk`'s scale
/// and the range documented on [`Transparency::percent`].
const OPACITY_MIN: f64 = 20.0;
const OPACITY_MAX: f64 = 100.0;
/// Font-size bounds. Generous either side of Scintilla's practical range
/// without inviting absurd values. Same numbers as `ui_gtk`.
const SIZE_MIN: u16 = 5;
const SIZE_MAX: u16 = 72;

/// Accessory-view geometry. Laid out top-down from a fixed height so the
/// rows cannot collide — the discipline m4a's Find panel was corrected to
/// after its Replace field overlapped its buttons.
const WIDTH: f64 = 420.0;
const HEIGHT: f64 = 300.0;
const ROW: f64 = 26.0;
const LABEL_W: f64 = 150.0;
const FIELD_X: f64 = 158.0;
/// Width of each of the three style checkboxes. Wide enough for
/// "Underline", the longest, and narrow enough that three fit the row
/// without overlapping.
const CHECK_W: f64 = 96.0;

/// The controls Save & Close reads back.
pub(crate) struct Controls {
    fg: Retained<NSColorWell>,
    bg: Retained<NSColorWell>,
    font: Retained<NSPopUpButton>,
    /// The *real* font-family names, in popup order.
    ///
    /// The popup's titles are sanitized for display, so they cannot be
    /// read back as the value — a face whose name carries bidi controls
    /// would round-trip as U+FFFD and quietly rewrite the user's stored
    /// font. Same value-vs-label split the workspace tree and the
    /// Find-in-Files dock use, and the reason this is a side table
    /// rather than `representedObject`: an index is all the read-back
    /// needs.
    font_faces: Vec<String>,
    size: Retained<NSTextField>,
    bold: Retained<NSButton>,
    italic: Retained<NSButton>,
    underline: Retained<NSButton>,
    transparency: Retained<NSButton>,
    opacity: Retained<NSSlider>,
}

/// Show the modal Style Configurator, then persist and apply on
/// Save & Close. A no-op on Cancel.
pub(crate) fn show() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(current) = with_state(|st| st.shell.styles.clone()) else {
        return;
    };
    let entry = current.effective_default();
    let transparency = current.effective_transparency();

    // A nested modal session services the GCD main-queue source, so it
    // takes the same freeze every other modal on this backend does.
    let _freeze = crate::DrainFreeze::new();
    let (alert, controls) = build_dialog(&entry, &transparency, mtm);
    // `NSAlertFirstButtonReturn` is 1000 — "Save & Close".
    if alert.runModal() != 1000 {
        return;
    }
    let new_styles = read_styles(&controls, &entry);

    with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.set_styles(new_styles);
        codepp_shell::UiPlatform::apply_default_style(&mut ui, &shell.styles);
        // After `apply_default_style`, never before: its
        // `SCI_STYLECLEARALL` wipes the lexer's per-style colours, and
        // `apply_lang` re-layers them over the new default. Keyword
        // classes survive the clear, so this repaints without re-lexing.
        let active_lang = shell.active().map_or(codepp_core::lang::L_TEXT, |t| t.lang);
        codepp_shell::UiPlatform::apply_lang(&mut ui, active_lang);
    });
}

/// Build the dialog and its controls.
///
/// Split from [`show`] so the wiring can be checked without entering a
/// modal session — `runModal` cannot return without a human. Same split
/// as `search::build_goto_alert` and `preferences::build_dialog`.
pub(crate) fn build_dialog(
    entry: &StyleEntry,
    transparency: &Transparency,
    mtm: MainThreadMarker,
) -> (Retained<NSAlert>, Controls) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Style Configurator"));
    alert.addButtonWithTitle(&NSString::from_str("Save & Close"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)),
    );
    let mut y = HEIGHT - ROW;

    // --- parity chrome: the three scope popups ---------------------
    add_label("Select theme:", y, &view, mtm);
    fixed_popup("Default (stylers.xml)", y, &view, mtm);
    y -= ROW;
    add_label("Language:", y, &view, mtm);
    fixed_popup("Global Styles", y, &view, mtm);
    y -= ROW;
    add_label("Style:", y, &view, mtm);
    fixed_popup("Default Style", y, &view, mtm);
    y -= ROW * 1.5;

    // --- the editable Default Style --------------------------------
    add_label("Foreground colour:", y, &view, mtm);
    let fg = color_well(&entry.fg, (0, 0, 0), y, mtm);
    view.addSubview(&fg);
    y -= ROW;

    add_label("Background colour:", y, &view, mtm);
    let bg = color_well(&entry.bg, (0xFF, 0xFF, 0xFF), y, mtm);
    view.addSubview(&bg);
    y -= ROW;

    add_label("Font:", y, &view, mtm);
    let font = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        NSRect::new(
            NSPoint::new(FIELD_X, y),
            NSSize::new(WIDTH - FIELD_X - 8.0, ROW - 4.0),
        ),
        false,
    );
    let font_faces = font_families(&entry.font_name, &installed_font_families(mtm));
    for family in &font_faces {
        // A font name reaches here from `styles.xml`, which is an
        // ordinary file a user can be handed — so it is display text
        // like any filename, and gets the same treatment.
        font.addItemWithTitle(&NSString::from_str(
            &codepp_shell::sanitize_str_for_display(family),
        ));
    }
    // Selected by *index*, not by title: the titles are sanitized, so a
    // hostile name would never match itself.
    if let Some(i) = font_faces.iter().position(|f| f == &entry.font_name) {
        font.selectItemAtIndex(isize::try_from(i).unwrap_or(0));
    }
    view.addSubview(&font);
    y -= ROW;

    add_label("Size:", y, &view, mtm);
    let size = NSTextField::initWithFrame(
        NSTextField::alloc(mtm),
        NSRect::new(NSPoint::new(FIELD_X, y), NSSize::new(60.0, ROW - 4.0)),
    );
    size.setStringValue(&NSString::from_str(&entry.font_size.to_string()));
    size.setBezelStyle(NSTextFieldBezelStyle::RoundedBezel);
    view.addSubview(&size);
    y -= ROW;

    // Explicit widths, not "the rest of the row": three checkboxes each
    // sized to the remaining width overlap, and which one a click lands
    // on then depends on the order they were added in. It resolved
    // correctly by accident — AppKit hit-tests subviews in reverse — but
    // an accident is not a layout.
    let bold = check("Bold", 0.0, y, CHECK_W, entry.bold, mtm);
    view.addSubview(&bold);
    let italic = check("Italic", CHECK_W, y, CHECK_W, entry.italic, mtm);
    view.addSubview(&italic);
    let underline = check("Underline", CHECK_W * 2.0, y, CHECK_W, entry.underline, mtm);
    view.addSubview(&underline);
    y -= ROW * 1.5;

    let transparency_check = check(
        "Enable window transparency",
        0.0,
        y,
        WIDTH,
        transparency.enabled,
        mtm,
    );
    view.addSubview(&transparency_check);
    y -= ROW;

    add_label("Opacity %:", y, &view, mtm);
    let opacity = NSSlider::initWithFrame(
        NSSlider::alloc(mtm),
        NSRect::new(
            NSPoint::new(FIELD_X, y),
            NSSize::new(WIDTH - FIELD_X - 8.0, ROW - 4.0),
        ),
    );
    opacity.setMinValue(OPACITY_MIN);
    opacity.setMaxValue(OPACITY_MAX);
    opacity.setDoubleValue(f64::from(transparency.percent).clamp(OPACITY_MIN, OPACITY_MAX));
    view.addSubview(&opacity);

    alert.setAccessoryView(Some(&view));
    (
        alert,
        Controls {
            fg,
            bg,
            font,
            font_faces,
            size,
            bold,
            italic,
            underline,
            transparency: transparency_check,
            opacity,
        },
    )
}

/// Read the controls into a fresh `Styles`. `prior` supplies the
/// fallbacks: the font face when the popup has no selection, and the
/// size when the field holds something that is not a size.
fn read_styles(c: &Controls, prior: &StyleEntry) -> Styles {
    // Read back from the side table by index — never from the item's
    // title, which is the sanitized label rather than the face.
    let font_name = usize::try_from(c.font.indexOfSelectedItem())
        .ok()
        .and_then(|i| c.font_faces.get(i))
        .cloned()
        .unwrap_or_else(|| prior.font_name.clone());
    Styles {
        default: Some(StyleEntry {
            font_name,
            font_size: parse_size(&c.size.stringValue().to_string())
                .unwrap_or(prior.font_size)
                .clamp(SIZE_MIN, SIZE_MAX),
            bold: c.bold.state() != 0,
            italic: c.italic.state() != 0,
            underline: c.underline.state() != 0,
            fg: well_to_hex(&c.fg, &prior.fg),
            bg: well_to_hex(&c.bg, &prior.bg),
        }),
        transparency: Some(Transparency {
            enabled: c.transparency.state() != 0,
            // `f64 as u8` saturates in Rust, and the slider is bounded
            // anyway; the clamp states the range rather than relying on
            // either fact.
            percent: c.opacity.doubleValue().clamp(OPACITY_MIN, OPACITY_MAX) as u8,
        }),
    }
}

/// A font size, or `None` for anything that is not one.
///
/// Out-of-range values are *not* rejected the way the Preferences
/// dialog's are — a size is clamped by the caller instead, because 200
/// means "as large as you can" in a way that 999 recent-files entries
/// does not. Anything unparseable still falls back to the previous
/// value rather than to a constant.
fn parse_size(text: &str) -> Option<u16> {
    text.trim().parse::<u16>().ok()
}

/// The colour a well holds, as canonical `RRGGBB`.
///
/// **Converted to sRGB first, and that is required rather than tidy.**
/// `NSColorWell` hands back an `NSColor` in whatever space the picker
/// produced — a catalog or pattern colour among them — and
/// `redComponent` raises an Objective-C exception for those, which
/// crosses the FFI boundary as undefined behaviour rather than a
/// catchable Rust panic. `usingColorSpace:` returns `None` when the
/// conversion is impossible, which is the case this falls back on.
fn well_to_hex(well: &NSColorWell, fallback: &str) -> String {
    let color = well.color();
    let Some(srgb) = color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace()) else {
        return fallback.to_string();
    };
    let to_u8 = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format_rgb_hex(
        to_u8(srgb.redComponent()),
        to_u8(srgb.greenComponent()),
        to_u8(srgb.blueComponent()),
    )
}

/// The font-family list to show, with `current` guaranteed present.
///
/// Pure, and tested, because the interesting case is the one a hands-on
/// demo is least likely to hit: a stored face that is not installed —
/// the "Courier New" default on a Mac without it. Dropping it would
/// silently rewrite the user's stored font the first time they touched
/// any other control in the dialog.
fn font_families(current: &str, installed: &[String]) -> Vec<String> {
    let mut families: Vec<String> = installed.to_vec();
    families.sort_unstable();
    families.dedup();
    if !current.is_empty() && !families.iter().any(|f| f == current) {
        families.insert(0, current.to_string());
    }
    families
}

fn installed_font_families(mtm: MainThreadMarker) -> Vec<String> {
    NSFontManager::sharedFontManager(mtm)
        .availableFontFamilies()
        .iter()
        .map(|f| f.to_string())
        .collect()
}

// --- small control constructors ---------------------------------------

fn add_label(text: &str, y: f64, view: &NSView, mtm: MainThreadMarker) {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFrame(NSRect::new(
        NSPoint::new(0.0, y),
        NSSize::new(LABEL_W, ROW - 6.0),
    ));
    view.addSubview(&label);
}

/// A popup with exactly one item — parity chrome for a scope that is not
/// selectable yet. Disabled rather than merely single-item, so it reads
/// as "fixed" rather than as a control that failed to populate.
fn fixed_popup(title: &str, y: f64, view: &NSView, mtm: MainThreadMarker) {
    let popup = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        NSRect::new(
            NSPoint::new(FIELD_X, y),
            NSSize::new(WIDTH - FIELD_X - 8.0, ROW - 4.0),
        ),
        false,
    );
    popup.addItemWithTitle(&NSString::from_str(title));
    popup.setEnabled(false);
    view.addSubview(&popup);
}

fn check(
    title: &str,
    x: f64,
    y: f64,
    width: f64,
    on: bool,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, ROW - 4.0)),
    );
    button.setButtonType(NSButtonType::Switch);
    button.setTitle(&NSString::from_str(title));
    button.setState(isize::from(on));
    button
}

fn color_well(
    hex: &str,
    fallback: (u8, u8, u8),
    y: f64,
    mtm: MainThreadMarker,
) -> Retained<NSColorWell> {
    let well = NSColorWell::initWithFrame(
        NSColorWell::alloc(mtm),
        NSRect::new(NSPoint::new(FIELD_X, y), NSSize::new(60.0, ROW - 4.0)),
    );
    let (r, g, b) = parse_rgb_hex(hex).unwrap_or(fallback);
    let scale = |c: u8| f64::from(c) / 255.0;
    well.setColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        scale(r),
        scale(g),
        scale(b),
        1.0,
    ));
    well
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uninstalled_stored_font_is_kept_and_listed_first() {
        let installed = vec!["Menlo".to_string(), "Helvetica".to_string()];
        let out = font_families("Courier New", &installed);
        assert_eq!(out, vec!["Courier New", "Helvetica", "Menlo"]);
    }

    #[test]
    fn an_installed_stored_font_is_not_duplicated() {
        let installed = vec!["Menlo".to_string(), "Helvetica".to_string()];
        let out = font_families("Menlo", &installed);
        assert_eq!(out, vec!["Helvetica", "Menlo"]);
    }

    #[test]
    fn duplicate_families_collapse_and_an_empty_current_adds_nothing() {
        let installed = vec![
            "Menlo".to_string(),
            "Menlo".to_string(),
            "Arial".to_string(),
        ];
        assert_eq!(font_families("", &installed), vec!["Arial", "Menlo"]);
    }

    #[test]
    fn a_size_is_parsed_or_declined_rather_than_guessed() {
        assert_eq!(parse_size("11"), Some(11));
        assert_eq!(parse_size("  9 "), Some(9));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("eleven"), None);
        assert_eq!(parse_size("-3"), None);
        assert_eq!(parse_size("10.5"), None);
        // Out of range parses; the caller clamps. See `parse_size`.
        assert_eq!(parse_size("400"), Some(400));
        assert_eq!(
            parse_size("400").map(|v| v.clamp(SIZE_MIN, SIZE_MAX)),
            Some(SIZE_MAX)
        );
        assert_eq!(
            parse_size("1").map(|v| v.clamp(SIZE_MIN, SIZE_MAX)),
            Some(SIZE_MIN)
        );
    }
}
