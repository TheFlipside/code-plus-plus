//! The 7-part status bar.
//!
//! Part order and text formats deliberately match the Win32 and GTK
//! backends (`ui_win32`'s `STATUS_PART_*` / `refresh_status_dynamic_parts`
//! and `ui_gtk`'s `status.rs`), so the three platforms read identically
//! and DESIGN.md §7.5's parity checklist is satisfiable by comparison
//! rather than by re-deciding the layout:
//!
//! | # | part     | content                          |
//! |---|----------|----------------------------------|
//! | 0 | language | active lexer's display name      |
//! | 1 | spring   | empty; absorbs slack on resize   |
//! | 2 | length   | `length: N   lines: N`           |
//! | 3 | cursor   | `Ln: N   Col: N   Pos: N`        |
//! | 4 | EOL      | `Windows (CR LF)` etc.           |
//! | 5 | encoding | `UTF-8` etc.                     |
//! | 6 | INS/OVR  | overtype indicator               |
//!
//! Win32 sizes its parts in pixels via `SB_SETPARTS` and GTK uses
//! character widths on a `GtkBox`. Cocoa gets the same behaviour from
//! springs-and-struts autoresizing: the fixed parts are laid out from
//! the right edge with `NSViewMinXMargin` so they stay pinned to it,
//! and the language label at the left is the only one that grows. That
//! reproduces "everything right of the language name stays put while
//! the gap grows" without Auto Layout constraints.
//!
//! Autoresizing rather than `NSStackView` is a deliberate choice: the
//! layout is a single row of fixed-width fields with one flexible gap,
//! which springs-and-struts expresses exactly, and it stays debuggable
//! from frame arithmetic alone.

use objc2::rc::Retained;
use objc2::MainThreadOnly;
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSColor, NSFont, NSLineBreakMode, NSTextField, NSView,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

/// Index of each part, matching `ui_win32`'s `STATUS_PART_*`.
const PART_LANG: usize = 0;
const PART_LENGTH: usize = 2;
const PART_CURSOR: usize = 3;
const PART_EOL: usize = 4;
const PART_ENCODING: usize = 5;
const PART_INSOVR: usize = 6;

/// Height of the bar in points.
pub const STATUS_BAR_HEIGHT: f64 = 22.0;

/// Point widths for the fixed parts, right-to-left from the window's
/// trailing edge. Sized to fit the widest realistic content at the
/// default small system font — `Ln: 999,999 Col: 999 Pos: 9,999,999`
/// for the cursor part, the longest EOL label (`Macintosh (CR)`), and
/// `OVR` for the indicator. Not the same numbers as Win32's
/// `STATUS_PART_*_W` or GTK's character counts, because the font metrics
/// differ; the resulting layout is what matches.
const W_INSOVR: f64 = 42.0;
const W_ENCODING: f64 = 96.0;
const W_EOL: f64 = 124.0;
const W_CURSOR: f64 = 210.0;
const W_LENGTH: f64 = 180.0;

/// Left inset for the language label, and the gap between fields.
const PART_INSET: f64 = 8.0;

/// Handle to the status bar.
///
/// `Clone` is a refcount bump on each field — this is handed out by
/// `CocoaUiState::split` on every drain, so it must stay cheap.
#[derive(Clone)]
pub struct StatusBar {
    /// The container, so visibility can be toggled wholesale.
    pub container: Retained<NSView>,
    labels: Vec<Retained<NSTextField>>,
}

impl StatusBar {
    /// Build the bar sized to `width` points. The caller positions the
    /// container; the fields inside autoresize with it.
    ///
    /// Parts start empty; the first `update_status` fills them.
    pub fn new(width: f64, mtm: MainThreadMarker) -> Self {
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(width, STATUS_BAR_HEIGHT),
            ),
        );
        // Grow horizontally with the window; stay pinned to the bottom.
        container.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);

        // Fixed parts, laid out right-to-left from the trailing edge.
        // Each is pinned to that edge (`ViewMinXMargin`) so the slack a
        // resize creates opens up on the left, in the spring.
        let fixed: [(usize, f64); 5] = [
            (PART_INSOVR, W_INSOVR),
            (PART_ENCODING, W_ENCODING),
            (PART_EOL, W_EOL),
            (PART_CURSOR, W_CURSOR),
            (PART_LENGTH, W_LENGTH),
        ];

        // Index-addressable slots; built in layout order below, then
        // sorted back into part order so `set` can index directly.
        let mut slots: Vec<Option<Retained<NSTextField>>> = (0..7).map(|_| None).collect();

        let mut right = width;
        for (part, part_width) in fixed {
            right -= part_width;
            let field = make_label(
                NSRect::new(
                    NSPoint::new(right, 0.0),
                    NSSize::new(part_width, STATUS_BAR_HEIGHT),
                ),
                mtm,
            );
            field.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            container.addSubview(&field);
            slots[part] = Some(field);
        }

        // The language label fills everything left of the fixed block —
        // it is both part 0 and, in effect, the spring. GTK models the
        // spring as a separate expanding label at index 1; here the
        // flexible width lives on the language field and index 1 is an
        // empty placeholder, which keeps the part indices identical
        // across backends (plugins address parts by index through
        // `NPPM_SETSTATUSBAR`).
        let lang_width = (right - PART_INSET).max(0.0);
        let lang = make_label(
            NSRect::new(
                NSPoint::new(PART_INSET, 0.0),
                NSSize::new(lang_width, STATUS_BAR_HEIGHT),
            ),
            mtm,
        );
        lang.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        container.addSubview(&lang);
        slots[PART_LANG] = Some(lang);

        // The spring placeholder: zero-width, never displayed, present
        // only so index 1 exists and the part numbering matches the
        // other two backends.
        let spring = make_label(
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, STATUS_BAR_HEIGHT)),
            mtm,
        );
        slots[1] = Some(spring);

        let labels = slots
            .into_iter()
            .map(|s| s.expect("every status part slot was filled"))
            .collect();

        Self { container, labels }
    }

    fn set(&self, part: usize, text: &str) {
        if let Some(label) = self.labels.get(part) {
            label.setStringValue(&NSString::from_str(text));
        }
    }

    /// Write the parts that only change on load / language / encoding
    /// changes.
    pub fn set_static_parts(&self, lang: &str, eol: &str, encoding: &str) {
        self.set(PART_LANG, lang);
        self.set(PART_EOL, eol);
        self.set(PART_ENCODING, encoding);
    }

    /// Write the parts that change on every caret move and edit.
    /// Mirrors `ui_win32::refresh_status_dynamic_parts` and `ui_gtk`'s
    /// equivalent, including the 1-based line/column display over
    /// Scintilla's 0-based values.
    pub fn set_dynamic_parts(
        &self,
        length: u64,
        lines: u64,
        caret_line: u64,
        caret_col: u64,
        pos: u64,
        overtype: bool,
    ) {
        self.set(
            PART_LENGTH,
            &format!(
                "length: {}   lines: {}",
                format_thousands(length),
                format_thousands(lines)
            ),
        );
        self.set(
            PART_CURSOR,
            &format!(
                "Ln: {}   Col: {}   Pos: {}",
                caret_line.saturating_add(1),
                caret_col.saturating_add(1),
                format_thousands(pos)
            ),
        );
        self.set(PART_INSOVR, if overtype { "OVR" } else { "INS" });
    }

    /// Plugin-driven override (`NPPM_SETSTATUSBAR`). The plugin owns
    /// that part's text until the next host update repaints it.
    pub fn set_plugin_part(&self, part: usize, text: &str) {
        self.set(part, text);
    }

    /// Whole-bar visibility, for `NPPM_HIDESTATUSBAR` and the View menu.
    pub fn set_hidden(&self, hidden: bool) {
        self.container.setHidden(hidden);
    }

    /// Whether the bar is currently hidden.
    pub fn is_hidden(&self) -> bool {
        self.container.isHidden()
    }
}

/// One status field: a non-editable, borderless, transparent
/// `NSTextField`, which is how AppKit spells "label".
fn make_label(frame: NSRect, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
    field.setEditable(false);
    field.setSelectable(false);
    field.setBordered(false);
    field.setBezeled(false);
    field.setDrawsBackground(false);
    field.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    field.setTextColor(Some(&NSColor::secondaryLabelColor()));
    // Long language names and plugin-supplied text must not be able to
    // force the window wider than the user sized it.
    if let Some(cell) = field.cell() {
        cell.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    }
    field
}

/// Group digits with separators, matching the other two bars.
fn format_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::format_thousands;

    /// Pins the shared formatting contract. Pure, so it runs in the
    /// default `cargo test` on every platform — no window server needed,
    /// unlike the widget-driving `cocoa_smoke` integration test.
    #[test]
    fn thousands_separator_matches_the_other_backends() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1000), "1,000");
        assert_eq!(format_thousands(1_234_567), "1,234,567");
        // Boundary that a naive `% 3 == 0` check gets wrong by
        // emitting a leading separator.
        assert_eq!(format_thousands(100), "100");
    }
}
