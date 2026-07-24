//! Settings → Style Configurator… dialog for the GTK backend.
//!
//! Mirrors the Win32 `handle_style_config_menu` / `show_style_config_dialog`
//! pair. It edits the one style scope wired on either backend so far — the
//! **Default Style** ([`codepp_core::styles::StyleEntry`], the `STYLE_DEFAULT`
//! index that `SCI_STYLECLEARALL` propagates to every other style) plus
//! window [`Transparency`]. Like Win32 there is **no live preview**: changes
//! accumulate in the controls and are applied only on "Save & Close".
//!
//! The theme / language / style combos are parity chrome ("Default
//! (stylers.xml)", "Global Styles", "Default Style") — they reflect that
//! only the default style is editable today, matching the `Styles` data
//! model. Native GTK pickers (`ColorButton`, a font-family combo, a size
//! spin) stand in for Win32's owner-drawn colour squares and custom popup.
//!
//! On Save & Close the controls are read back into a fresh `Styles`, then
//! `Shell::set_styles` persists it and `apply_default_style` + `apply_lang`
//! push it to the live editor — the same sequence, and the same reasoning
//! for the trailing `apply_lang` (re-layer the lexer's per-style colours
//! that `SCI_STYLECLEARALL` just wiped), as the Win32 handler. That whole
//! data path is already cross-platform.

use std::str::FromStr;

use codepp_core::styles::{format_rgb_hex, parse_rgb_hex, StyleEntry, Styles, Transparency};
use gtk::prelude::*;

use crate::state::with_state;

/// Opacity slider bounds — 100 % opaque down to a 20 % floor, below which
/// the editor becomes unreadable. Matches the Win32 slider and the range
/// documented on [`Transparency::percent`].
const OPACITY_MIN: f64 = 20.0;
const OPACITY_MAX: f64 = 100.0;
/// Font-size spin bounds. Generous either side of Scintilla's practical
/// range without inviting absurd values.
const SIZE_MIN: f64 = 5.0;
const SIZE_MAX: f64 = 72.0;

/// The controls whose values are read back on Save & Close. Held so
/// [`build_content`] can populate the dialog while [`read_styles`] later
/// reads it, keeping [`show`] short.
struct Controls {
    fg_button: gtk::ColorButton,
    bg_button: gtk::ColorButton,
    font_combo: gtk::ComboBoxText,
    size_spin: gtk::SpinButton,
    bold: gtk::CheckButton,
    italic: gtk::CheckButton,
    underline: gtk::CheckButton,
    transp_check: gtk::CheckButton,
    transp_scale: gtk::Scale,
}

/// Show the modal Style Configurator. Reads the current `Styles`, presents
/// the Default-Style controls, and on "Save & Close" persists + applies the
/// edited style. A no-op on Cancel / Esc / close.
pub(crate) fn show(window: &gtk::Window) {
    let Some(current) = with_state(|st| st.shell.styles.clone()) else {
        return;
    };
    let entry = current.effective_default();
    let transparency = current.effective_transparency();

    let dialog = gtk::Dialog::with_buttons(
        Some("Style Configurator"),
        Some(window),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[
            ("_Cancel", gtk::ResponseType::Cancel),
            ("_Save & Close", gtk::ResponseType::Accept),
        ],
    );
    dialog.set_default_response(gtk::ResponseType::Accept);
    let content = dialog.content_area();
    content.set_spacing(10);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(10);
    content.set_margin_end(10);

    let controls = build_content(window, &content, &entry, &transparency);

    dialog.show_all();
    let response = dialog.run();
    let result = (response == gtk::ResponseType::Accept).then(|| read_styles(&controls, &entry));

    // SAFETY: created here and never handed out — same idiom as the other
    // GTK modals (Preferences, About, Plugin Manager).
    unsafe {
        dialog.destroy();
    }

    let Some(new_styles) = result else {
        return;
    };
    // Persist, then push to the live editor. Mirrors Win32's
    // `handle_style_config_menu`: `apply_default_style`'s `SCI_STYLECLEARALL`
    // wipes the lexer's per-style colours, so `apply_lang` re-layers them on
    // top of the new default scheme (keyword classes survive the clear, so
    // this only repaints — no re-lex).
    with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.set_styles(new_styles);
        codepp_shell::UiPlatform::apply_default_style(&mut ui, &shell.styles);
        let active_lang = shell.active().map_or(codepp_core::lang::L_TEXT, |t| t.lang);
        codepp_shell::UiPlatform::apply_lang(&mut ui, active_lang);
    });
}

/// Populate `content` with the dialog's widgets — the theme/language/style
/// chrome and the Default-Style controls — seeded from `entry` /
/// `transparency`. Returns the [`Controls`] [`read_styles`] reads back.
fn build_content(
    window: &gtk::Window,
    content: &gtk::Box,
    entry: &StyleEntry,
    transparency: &Transparency,
) -> Controls {
    // --- Theme row (chrome) ---
    let theme_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    theme_row.pack_start(&gtk::Label::new(Some("Select theme:")), false, false, 0);
    let theme_combo = gtk::ComboBoxText::new();
    theme_combo.append_text("Default (stylers.xml)");
    theme_combo.set_active(Some(0));
    theme_row.pack_start(&theme_combo, false, false, 0);
    content.pack_start(&theme_row, false, false, 0);

    // --- Language + Style column (chrome) beside the style controls ---
    let columns = gtk::Box::new(gtk::Orientation::Horizontal, 12);

    let left = gtk::Box::new(gtk::Orientation::Vertical, 6);
    left.pack_start(&left_label("Language:"), false, false, 0);
    let lang_combo = gtk::ComboBoxText::new();
    lang_combo.append_text("Global Styles");
    lang_combo.set_active(Some(0));
    left.pack_start(&lang_combo, false, false, 0);
    left.pack_start(&left_label("Style:"), false, false, 0);
    let style_list = gtk::ListBox::new();
    let style_row = gtk::ListBoxRow::new();
    style_row.add(&gtk::Label::new(Some("Default Style")));
    style_list.add(&style_row);
    style_list.select_row(Some(&style_row));
    let style_scroll = gtk::ScrolledWindow::builder()
        .min_content_width(150)
        .build();
    style_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    style_scroll.add(&style_list);
    left.pack_start(&style_scroll, true, true, 0);
    columns.pack_start(&left, false, false, 0);

    // --- Right: the Default-Style controls, in a grid ---
    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(8);

    let (r, g, b) = parse_rgb_hex(&entry.fg).unwrap_or((0, 0, 0));
    let fg_button = color_button(r, g, b);
    grid.attach(&left_label("Foreground colour:"), 0, 0, 1, 1);
    grid.attach(&fg_button, 1, 0, 1, 1);

    let (r, g, b) = parse_rgb_hex(&entry.bg).unwrap_or((0xFF, 0xFF, 0xFF));
    let bg_button = color_button(r, g, b);
    grid.attach(&left_label("Background colour:"), 0, 1, 1, 1);
    grid.attach(&bg_button, 1, 1, 1, 1);

    let font_combo = build_font_combo(window, &entry.font_name);
    grid.attach(&left_label("Font:"), 0, 2, 1, 1);
    grid.attach(&font_combo, 1, 2, 1, 1);

    let size_spin = gtk::SpinButton::with_range(SIZE_MIN, SIZE_MAX, 1.0);
    size_spin.set_value(f64::from(entry.font_size));
    grid.attach(&left_label("Size:"), 0, 3, 1, 1);
    grid.attach(&size_spin, 1, 3, 1, 1);

    let style_flags = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let bold = gtk::CheckButton::with_label("Bold");
    bold.set_active(entry.bold);
    let italic = gtk::CheckButton::with_label("Italic");
    italic.set_active(entry.italic);
    let underline = gtk::CheckButton::with_label("Underline");
    underline.set_active(entry.underline);
    style_flags.pack_start(&bold, false, false, 0);
    style_flags.pack_start(&italic, false, false, 0);
    style_flags.pack_start(&underline, false, false, 0);
    grid.attach(&style_flags, 0, 4, 2, 1);

    let transp_check = gtk::CheckButton::with_label("Enable window transparency");
    transp_check.set_active(transparency.enabled);
    grid.attach(&transp_check, 0, 5, 2, 1);
    let transp_scale =
        gtk::Scale::with_range(gtk::Orientation::Horizontal, OPACITY_MIN, OPACITY_MAX, 1.0);
    transp_scale.set_value(f64::from(transparency.percent).clamp(OPACITY_MIN, OPACITY_MAX));
    transp_scale.set_hexpand(true);
    transp_scale.set_sensitive(transparency.enabled);
    // Keep the slider greyed unless transparency is enabled.
    let scale_for_toggle = transp_scale.clone();
    transp_check.connect_toggled(move |c| scale_for_toggle.set_sensitive(c.is_active()));
    grid.attach(&left_label("Opacity %:"), 0, 6, 1, 1);
    grid.attach(&transp_scale, 1, 6, 1, 1);

    columns.pack_start(&grid, true, true, 0);
    content.pack_start(&columns, true, true, 0);

    Controls {
        fg_button,
        bg_button,
        font_combo,
        size_spin,
        bold,
        italic,
        underline,
        transp_check,
        transp_scale,
    }
}

/// Read the controls into a fresh `Styles`. `prior` supplies the fallback
/// font face if the combo has no active entry.
fn read_styles(c: &Controls, prior: &StyleEntry) -> Styles {
    let font_name = c
        .font_combo
        .active_text()
        .map_or_else(|| prior.font_name.clone(), |s| s.to_string());
    Styles {
        default: Some(StyleEntry {
            font_name,
            font_size: c
                .size_spin
                .value_as_int()
                .clamp(SIZE_MIN as i32, SIZE_MAX as i32) as u16,
            bold: c.bold.is_active(),
            italic: c.italic.is_active(),
            underline: c.underline.is_active(),
            fg: rgba_to_hex(&c.fg_button.rgba()),
            bg: rgba_to_hex(&c.bg_button.rgba()),
        }),
        transparency: Some(Transparency {
            enabled: c.transp_check.is_active(),
            percent: c.transp_scale.value().clamp(OPACITY_MIN, OPACITY_MAX) as u8,
        }),
    }
}

/// A left-aligned label — the dialog's fields read left-to-right.
fn left_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label
}

/// A `ColorButton` seeded to an `RRGGBB` colour.
fn color_button(r: u8, g: u8, b: u8) -> gtk::ColorButton {
    let button = gtk::ColorButton::new();
    // Seed via the canonical `#rrggbb` string so we don't depend on the
    // exact `RGBA` constructor signature across gdk versions.
    if let Ok(rgba) = gtk::gdk::RGBA::from_str(&format!("#{}", format_rgb_hex(r, g, b))) {
        button.set_rgba(&rgba);
    }
    button
}

/// Convert a picked `RGBA` back to the canonical `RRGGBB` hex form. The
/// alpha channel is ignored — the Default Style has no per-glyph alpha;
/// window transparency is the separate opacity slider.
fn rgba_to_hex(rgba: &gtk::gdk::RGBA) -> String {
    let to_u8 = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format_rgb_hex(to_u8(rgba.red()), to_u8(rgba.green()), to_u8(rgba.blue()))
}

/// Build the font-family combo: the system families, sorted, with `current`
/// selected. `current` is inserted at the top when it isn't an installed
/// family (e.g. the "Courier New" default on a Linux box without it), so the
/// user's stored face is always preserved and shown even if unavailable.
fn build_font_combo(window: &gtk::Window, current: &str) -> gtk::ComboBoxText {
    let combo = gtk::ComboBoxText::new();
    let mut families: Vec<String> = window
        .pango_context()
        .list_families()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    families.sort_unstable();
    families.dedup();

    // Either `current` is an installed family (found in the loop below) or it
    // is not (prepended at index 0). The two branches are mutually exclusive,
    // so the prepend offsets every family index by one and the loop's own
    // match can't fire in that run.
    let prepended = !current.is_empty() && !families.iter().any(|f| f == current);
    let mut active = None;
    if prepended {
        combo.append_text(current);
        active = Some(0);
    }
    for (i, name) in families.iter().enumerate() {
        combo.append_text(name);
        if name == current {
            active = Some(u32::try_from(i).unwrap_or(0) + u32::from(prepended));
        }
    }
    combo.set_active(active.or(Some(0)));
    combo
}
