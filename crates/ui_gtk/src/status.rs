//! The 7-part status bar.
//!
//! Part order, widths and text formats deliberately match the Win32
//! backend (`ui_win32`'s `STATUS_PART_*` constants and
//! `refresh_status_dynamic_parts`), so the two platforms read
//! identically and DESIGN.md §7.5's parity checklist is satisfiable by
//! comparison rather than by re-deciding the layout:
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
//! Win32 sizes its parts in pixels via `SB_SETPARTS`. GTK has no such
//! control, so the equivalent is a horizontal box: fixed-width labels
//! for the sized parts and one `hexpand` label as the spring, which
//! reproduces the same "everything right of the language name stays put
//! while the gap grows" behaviour without hard-coding pixel maths.

use gtk::prelude::*;

/// Index of each part, matching `ui_win32`'s `STATUS_PART_*`.
const PART_LANG: usize = 0;
const PART_LENGTH: usize = 2;
const PART_CURSOR: usize = 3;
const PART_EOL: usize = 4;
const PART_ENCODING: usize = 5;
const PART_INSOVR: usize = 6;

/// Character widths for the fixed parts. Chosen to fit the widest
/// realistic content at the default font — `Ln: 999,999 Col: 999 Pos:
/// 9,999,999` for the cursor part, and the longest EOL label
/// (`Macintosh (CR)`) plus padding for the rest. GTK measures in
/// characters rather than the pixels Win32 uses, so these are not the
/// same numbers as `STATUS_PART_*_W`, but they produce the same layout.
const W_LANG: i32 = 22;
const W_LENGTH: i32 = 26;
const W_CURSOR: i32 = 30;
const W_EOL: i32 = 18;
const W_ENCODING: i32 = 14;
const W_INSOVR: i32 = 5;

/// Handle to the status bar's widgets.
///
/// `Clone` is a refcount bump on each label — this is handed out by
/// `GtkUiState::split` on every drain, so it must stay cheap.
#[derive(Clone)]
pub struct StatusBar {
    /// The container, so visibility can be toggled wholesale.
    pub container: gtk::Box,
    labels: Vec<gtk::Label>,
}

impl StatusBar {
    /// Build the bar. Parts start empty; the first `update_status`
    /// fills them.
    pub fn new() -> Self {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let mut labels = Vec::with_capacity(7);
        // (width_chars, expands) per part. The spring at index 1 is the
        // only expanding one.
        let spec: [(i32, bool); 7] = [
            (W_LANG, false),
            (0, true),
            (W_LENGTH, false),
            (W_CURSOR, false),
            (W_EOL, false),
            (W_ENCODING, false),
            (W_INSOVR, false),
        ];
        for (width, expands) in spec {
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            if width > 0 {
                label.set_width_chars(width);
            }
            label.set_hexpand(expands);
            // Long paths and language names must not be allowed to
            // force the window wider than the user sized it.
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            container.pack_start(&label, expands, true, 0);
            labels.push(label);
        }
        Self { container, labels }
    }

    fn set(&self, part: usize, text: &str) {
        if let Some(label) = self.labels.get(part) {
            label.set_text(text);
        }
    }

    /// Write the parts that only change on load / language / encoding
    /// changes.
    pub fn set_static_parts(&self, lang: &str, eol: &str, encoding: &str) {
        self.set(PART_LANG, &format!("  {lang}"));
        self.set(PART_EOL, &format!("  {eol}"));
        self.set(PART_ENCODING, &format!("  {encoding}"));
    }

    /// Write the parts that change on every caret move and edit.
    /// Mirrors `ui_win32::refresh_status_dynamic_parts`, including the
    /// 1-based line/column display over Scintilla's 0-based values.
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
                "  length: {}   lines: {}",
                format_thousands(length),
                format_thousands(lines)
            ),
        );
        self.set(
            PART_CURSOR,
            &format!(
                "  Ln: {}   Col: {}   Pos: {}",
                caret_line.saturating_add(1),
                caret_col.saturating_add(1),
                format_thousands(pos)
            ),
        );
        self.set(PART_INSOVR, if overtype { "  OVR" } else { "  INS" });
    }

    /// Plugin-driven override (`NPPM_SETSTATUSBAR`). The plugin owns
    /// that part's text until the next host update repaints it.
    pub fn set_plugin_part(&self, part: usize, text: &str) {
        self.set(part, text);
    }
}

/// Group digits with thin separators, matching the Win32 bar.
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

    #[test]
    fn thousands_separator_matches_win32_formatting() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1000), "1,000");
        assert_eq!(format_thousands(1_234_567), "1,234,567");
        // Boundary that a naive `% 3 == 0` check gets wrong by
        // emitting a leading separator.
        assert_eq!(format_thousands(100), "100");
    }
}
