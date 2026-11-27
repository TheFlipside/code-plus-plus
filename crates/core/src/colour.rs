//! Colour arithmetic for the light and dark schemes (DESIGN.md §7.4, the
//! dark-mode entry): which scheme the editor is drawn in, WCAG contrast
//! between two colours, and moving a colour's lightness until it reads
//! against a background.
//!
//! Colours here are `0xRRGGBB`, red most significant, as `styles.xml` and
//! UDL files write them. Scintilla wants `0x00BBGGRR`; [`to_colorref`]
//! swaps the two outer bytes, and is its own inverse.

/// Whether the editor is drawn light or dark.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ColourScheme {
    /// Dark text on a light background. What Code++ draws until a
    /// backend reports otherwise.
    #[default]
    Light,
    /// Light text on a dark background.
    Dark,
}

impl ColourScheme {
    /// `"light"` or `"dark"`, for logs and for the Style Configurator's
    /// label of which colours it edits.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            ColourScheme::Light => "light",
            ColourScheme::Dark => "dark",
        }
    }
}

/// The least contrast text must have against its background: WCAG 2's
/// level AA for body text.
pub const MIN_TEXT_CONTRAST: f64 = 4.5;

/// `rgb` as Scintilla's `0x00BBGGRR`, or back: the outer bytes swapped.
#[must_use]
pub const fn to_colorref(rgb: u32) -> u32 {
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    (b << 16) | (g << 8) | r
}

/// `(r, g, b)` packed as `0xRRGGBB`.
#[must_use]
pub const fn pack(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// The three components of `0xRRGGBB`.
#[must_use]
pub const fn unpack(rgb: u32) -> (u8, u8, u8) {
    (
        ((rgb >> 16) & 0xFF) as u8,
        ((rgb >> 8) & 0xFF) as u8,
        (rgb & 0xFF) as u8,
    )
}

/// WCAG 2's relative luminance of `0xRRGGBB`, from 0 (black) to 1
/// (white).
#[must_use]
pub fn relative_luminance(rgb: u32) -> f64 {
    let channel = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = unpack(rgb);
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// WCAG 2's contrast ratio between two colours, from 1 (the same
/// luminance) to 21 (black on white). Symmetric.
#[must_use]
pub fn contrast_ratio(a: u32, b: u32) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// `fg` laid over `bg` at `percent` of its own strength, channel by
/// channel: 100 gives `fg`, 0 gives `bg`. `percent` above 100 counts as
/// 100.
#[must_use]
pub fn mix(fg: u32, bg: u32, percent: u32) -> u32 {
    let p = percent.min(100);
    let (fr, fg_, fb) = unpack(fg);
    let (br, bg_, bb) = unpack(bg);
    let ch = |f: u8, b: u8| -> u8 {
        let v = (u32::from(f) * p + u32::from(b) * (100 - p)) / 100;
        // At most 255: a weighted mean of two bytes.
        debug_assert!(v <= 255, "a mean of two bytes is a byte");
        u8::try_from(v).unwrap_or(u8::MAX)
    };
    pack(ch(fr, br), ch(fg_, bg_), ch(fb, bb))
}

/// Whether `bg` is a dark background: one against which white text has
/// more contrast than black.
#[must_use]
pub fn is_dark(bg: u32) -> bool {
    contrast_ratio(0xFF_FF_FF, bg) > contrast_ratio(0, bg)
}

/// `rgb` as HSL: hue in degrees, saturation and lightness from 0 to 1.
fn to_hsl(rgb: u32) -> (f64, f64, f64) {
    let (red, green, blue) = unpack(rgb);
    let (red, green, blue) = (
        f64::from(red) / 255.0,
        f64::from(green) / 255.0,
        f64::from(blue) / 255.0,
    );
    let max = red.max(green).max(blue);
    let min = red.min(green).min(blue);
    let lightness = f64::midpoint(max, min);
    let chroma = max - min;
    if chroma == 0.0 {
        return (0.0, 0.0, lightness);
    }
    let saturation = chroma / (1.0 - (2.0 * lightness - 1.0).abs());
    let hue = if (max - red).abs() < f64::EPSILON {
        60.0 * ((green - blue) / chroma).rem_euclid(6.0)
    } else if (max - green).abs() < f64::EPSILON {
        60.0 * ((blue - red) / chroma + 2.0)
    } else {
        60.0 * ((red - green) / chroma + 4.0)
    };
    (hue, saturation.clamp(0.0, 1.0), lightness)
}

/// HSL back to `0xRRGGBB`, each channel rounded.
fn from_hsl(hue: f64, saturation: f64, lightness: f64) -> u32 {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = hue.rem_euclid(360.0) / 60.0;
    let second = chroma * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
    let (red, green, blue) = match sector {
        s if s < 1.0 => (chroma, second, 0.0),
        s if s < 2.0 => (second, chroma, 0.0),
        s if s < 3.0 => (0.0, chroma, second),
        s if s < 4.0 => (0.0, second, chroma),
        s if s < 5.0 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let offset = lightness - chroma / 2.0;
    let byte = |channel: f64| -> u8 {
        let scaled = ((channel + offset) * 255.0).round().clamp(0.0, 255.0);
        // In 0..=255 after the clamp.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let value = scaled as u8;
        value
    };
    pack(byte(red), byte(green), byte(blue))
}

/// `rgb` with its HSL lightness turned over, hue and saturation kept: a
/// light background becomes a dark one of the same hue, and the other
/// way round.
#[must_use]
pub fn invert_lightness(rgb: u32) -> u32 {
    let (hue, saturation, lightness) = to_hsl(rgb);
    from_hsl(hue, saturation, 1.0 - lightness)
}

/// `fg`, or the nearest colour of its hue and saturation that has
/// [`MIN_TEXT_CONTRAST`] against `bg`.
///
/// Returned unchanged when it already reads. Otherwise its HSL lightness
/// moves away from `bg` — up on a dark background, down on a light one —
/// by the least that reaches the contrast. Where no lightness reaches it,
/// which happens only on a mid-grey background, the result is black or
/// white, whichever has more. So the result always has the contrast when
/// black or white has it, and the best contrast to be had otherwise.
#[must_use]
pub fn readable_on(fg: u32, bg: u32) -> u32 {
    if contrast_ratio(fg, bg) >= MIN_TEXT_CONTRAST {
        return fg;
    }
    let upward = is_dark(bg);
    let extreme = if upward { 0xFF_FF_FF } else { 0 };
    if contrast_ratio(extreme, bg) < MIN_TEXT_CONTRAST {
        return extreme;
    }
    let (hue, saturation, start) = to_hsl(fg);
    let reads = |lightness: f64| {
        contrast_ratio(from_hsl(hue, saturation, lightness), bg) >= MIN_TEXT_CONTRAST
    };
    // Moving away from `bg` from where `fg` is, contrast first falls (while
    // `fg` is on `bg`'s side of it) and then rises, so whether a lightness
    // reads changes once along the way: a binary search finds the change.
    let (mut near, mut far) = if upward { (start, 1.0) } else { (start, 0.0) };
    for _ in 0..32 {
        let mid = f64::midpoint(near, far);
        if reads(mid) {
            far = mid;
        } else {
            near = mid;
        }
    }
    let found = from_hsl(hue, saturation, far);
    if contrast_ratio(found, bg) >= MIN_TEXT_CONTRAST {
        found
    } else {
        extreme
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorref_swaps_the_outer_bytes_both_ways() {
        assert_eq!(to_colorref(0x12_34_56), 0x56_34_12);
        assert_eq!(to_colorref(to_colorref(0xAB_CD_EF)), 0xAB_CD_EF);
    }

    #[test]
    fn contrast_matches_the_wcag_endpoints() {
        assert!((contrast_ratio(0, 0xFF_FF_FF) - 21.0).abs() < 1e-9);
        assert!((contrast_ratio(0x77_77_77, 0x77_77_77) - 1.0).abs() < 1e-9);
        assert!((contrast_ratio(0xFF_FF_FF, 0) - contrast_ratio(0, 0xFF_FF_FF)).abs() < 1e-12);
        // WCAG's own worked figure: #777 on white is about 4.48:1.
        let grey = contrast_ratio(0x77_77_77, 0xFF_FF_FF);
        assert!((4.47..4.49).contains(&grey), "{grey}");
    }

    #[test]
    fn mix_weights_the_two_colours() {
        assert_eq!(mix(0xFF_00_00, 0xFF_FF_FF, 100), 0xFF_00_00);
        assert_eq!(mix(0xFF_00_00, 0xFF_FF_FF, 0), 0xFF_FF_FF);
        assert_eq!(mix(0xFF_00_00, 0, 50), 0x7F_00_00);
        assert_eq!(mix(0xFF_00_00, 0, 250), 0xFF_00_00);
    }

    #[test]
    fn darkness_is_decided_by_the_better_text_colour() {
        assert!(is_dark(0x1E_1E_1E));
        assert!(is_dark(0x35_35_35));
        assert!(!is_dark(0xF6_F5_F4));
        assert!(!is_dark(0xFF_FF_FF));
    }

    #[test]
    fn hsl_round_trips_without_drift() {
        for rgb in [
            0, 0xFF_FF_FF, 0x80_00_80, 0x12_9A_E4, 0xC0_CA_33, 0x6A_99_55,
        ] {
            let (hue, saturation, lightness) = to_hsl(rgb);
            assert_eq!(from_hsl(hue, saturation, lightness), rgb, "{rgb:06X}");
        }
    }

    #[test]
    fn inverting_lightness_turns_white_black_and_keeps_hue() {
        assert_eq!(invert_lightness(0xFF_FF_FF), 0);
        assert_eq!(invert_lightness(0), 0xFF_FF_FF);
        // A pale yellow highlight becomes a dark yellow one.
        let dark = invert_lightness(0xFF_FF_C0);
        let (r, g, b) = unpack(dark);
        assert!(r == g && b < r, "{dark:06X}");
        assert!(is_dark(dark));
    }

    #[test]
    fn readable_on_leaves_a_colour_that_reads() {
        assert_eq!(readable_on(0x00_00_FF, 0xFF_FF_FF), 0x00_00_FF);
        assert_eq!(readable_on(0xDC_DC_DC, 0x1E_1E_1E), 0xDC_DC_DC);
    }

    #[test]
    fn readable_on_moves_a_colour_that_does_not_and_keeps_its_hue() {
        // Notepad++'s dark-blue keyword colour on a dark background.
        let bg = 0x1E_1E_1E;
        let out = readable_on(0x00_00_80, bg);
        assert!(contrast_ratio(out, bg) >= MIN_TEXT_CONTRAST, "{out:06X}");
        let (r, g, b) = unpack(out);
        assert!(b > r && b > g, "still blue: {out:06X}");
        // A pale colour on white moves darker.
        let out = readable_on(0xFF_FF_C0, 0xFF_FF_FF);
        assert!(
            contrast_ratio(out, 0xFF_FF_FF) >= MIN_TEXT_CONTRAST,
            "{out:06X}"
        );
        assert!(!is_dark(0xFF_FF_FF) && relative_luminance(out) < relative_luminance(0xFF_FF_C0));
    }

    #[test]
    fn readable_on_moves_no_further_than_it_must() {
        let bg = 0x1E_1E_1E;
        let out = readable_on(0x80_00_00, bg);
        assert!(contrast_ratio(out, bg) >= MIN_TEXT_CONTRAST);
        // Not pushed all the way to white.
        assert!(
            contrast_ratio(out, bg) < MIN_TEXT_CONTRAST + 0.3,
            "{out:06X}"
        );
    }

    #[test]
    fn readable_on_reads_against_every_grey_it_can() {
        for grey in (0u8..=0xFF).step_by(5) {
            let bg = pack(grey, grey, grey);
            for fg in [
                0, 0xFF_FF_FF, 0x80_00_80, 0x00_80_00, 0xFF_80_00, 0x80_80_80, bg,
            ] {
                let out = readable_on(fg, bg);
                let best = contrast_ratio(0, bg).max(contrast_ratio(0xFF_FF_FF, bg));
                let want = MIN_TEXT_CONTRAST.min(best);
                assert!(
                    contrast_ratio(out, bg) + 1e-9 >= want,
                    "fg {fg:06X} on {bg:06X} gave {out:06X}"
                );
            }
        }
    }
}
