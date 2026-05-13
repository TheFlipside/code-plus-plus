//! End-of-line style detection and bookkeeping.
//!
//! Notepad++ tracks the EOL style per buffer and preserves it on save
//! unless the user explicitly converts. Code++ matches that behaviour.
//!
//! See DESIGN.md §5.2.

use serde::{Deserialize, Serialize};

/// End-of-line style for a buffer.
///
/// Serialized as the human-readable [`label`](Self::label) string
/// (`"LF"` / `"CRLF"` / `"CR"` / `"Mixed"`) so `session.xml` is
/// readable. An unrecognised label deserializes to the default (`Lf`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum Eol {
    /// `\n` only — Unix, modern Linux, modern macOS.
    #[default]
    Lf,
    /// `\r\n` — Windows, DOS, HTTP, most network protocols.
    CrLf,
    /// `\r` only — pre-OS X Macintosh.
    Cr,
    /// File contains a mix of EOL styles. Each line keeps its original
    /// ending on save; the status bar displays a warning glyph.
    Mixed,
}

impl Eol {
    /// The byte sequence for this EOL when writing a new line.
    /// `Mixed` falls back to LF — the per-line preservation is a higher-
    /// level concern when re-emitting an existing file.
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            // Mixed defers to LF as the "preserve original on
            // re-write, but if we need to pick one here, use LF"
            // contract. Same body as `Lf`, kept as a separate
            // arm so the rationale stays at the call-site rather
            // than relying on `_` to swallow the variant.
            Eol::Lf | Eol::Mixed => b"\n",
            Eol::CrLf => b"\r\n",
            Eol::Cr => b"\r",
        }
    }

    /// Human-readable label for the status bar and `session.xml`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Eol::Lf => "LF",
            Eol::CrLf => "CRLF",
            Eol::Cr => "CR",
            Eol::Mixed => "Mixed",
        }
    }

    /// Long human-readable label, in the form Notepad++ shows in
    /// its status bar: "Windows (CR LF)", "Unix (LF)",
    /// "Macintosh (CR)". For [`Self::Mixed`] there's no canonical
    /// long form (the file uses more than one EOL); we surface
    /// "Mixed" as-is so the user sees the unusual state without
    /// being told it's a specific OS convention.
    #[must_use]
    pub const fn long_label(self) -> &'static str {
        match self {
            Eol::Lf => "Unix (LF)",
            Eol::CrLf => "Windows (CR LF)",
            Eol::Cr => "Macintosh (CR)",
            Eol::Mixed => "Mixed",
        }
    }

    /// Inverse of [`label`](Self::label). Unknown values default to
    /// `Lf` so a hand-edited session.xml doesn't crash the editor.
    /// A warning is logged via `tracing` so a corrupted session is
    /// diagnosable from the log.
    pub fn from_label(s: &str) -> Eol {
        match s {
            "LF" => Eol::Lf,
            "CRLF" => Eol::CrLf,
            "CR" => Eol::Cr,
            "Mixed" => Eol::Mixed,
            other => {
                tracing::warn!(label = %other, "unknown EOL label; defaulting to LF");
                Eol::Lf
            }
        }
    }
}

impl From<Eol> for String {
    fn from(e: Eol) -> Self {
        e.label().to_owned()
    }
}

impl From<String> for Eol {
    fn from(s: String) -> Self {
        Eol::from_label(&s)
    }
}

/// Detect the dominant EOL style of `bytes` by counting line endings in
/// the first 64 KiB. Files with mixed endings (e.g., a CRLF file with
/// stray bare LFs) are reported as `Mixed`; truly empty or single-line
/// files default to `Lf` (the file has no EOL evidence to suggest
/// otherwise, and LF is the modern default).
///
/// The 64 KiB cap is to keep detection bounded for huge files; it is
/// extremely rare for the first 64 KiB to disagree with the rest of the
/// file on dominant EOL.
#[must_use]
pub fn detect(bytes: &[u8]) -> Eol {
    let scan = if bytes.len() > 65_536 {
        &bytes[..65_536]
    } else {
        bytes
    };

    let mut crlf = 0usize;
    let mut bare_lf = 0usize;
    let mut bare_cr = 0usize;

    let mut i = 0;
    while i < scan.len() {
        match scan[i] {
            b'\r' => {
                if scan.get(i + 1) == Some(&b'\n') {
                    crlf += 1;
                    i += 2;
                    continue;
                }
                bare_cr += 1;
            }
            b'\n' => bare_lf += 1,
            _ => {}
        }
        i += 1;
    }

    let kinds = [crlf > 0, bare_lf > 0, bare_cr > 0]
        .iter()
        .filter(|present| **present)
        .count();

    match kinds {
        0 => Eol::Lf,
        1 if crlf > 0 => Eol::CrLf,
        1 if bare_lf > 0 => Eol::Lf,
        1 if bare_cr > 0 => Eol::Cr,
        _ => Eol::Mixed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_defaults_to_lf() {
        assert_eq!(detect(b""), Eol::Lf);
    }

    #[test]
    fn single_line_no_newline_defaults_to_lf() {
        assert_eq!(detect(b"hello world"), Eol::Lf);
    }

    #[test]
    fn pure_lf() {
        assert_eq!(detect(b"a\nb\nc\n"), Eol::Lf);
    }

    #[test]
    fn pure_crlf() {
        assert_eq!(detect(b"a\r\nb\r\nc\r\n"), Eol::CrLf);
    }

    #[test]
    fn pure_cr_classic_mac() {
        assert_eq!(detect(b"a\rb\rc\r"), Eol::Cr);
    }

    #[test]
    fn mixed_lf_and_crlf() {
        assert_eq!(detect(b"a\nb\r\nc\n"), Eol::Mixed);
    }

    #[test]
    fn mixed_cr_and_lf() {
        assert_eq!(detect(b"a\rb\nc\r"), Eol::Mixed);
    }

    #[test]
    fn crlf_not_counted_as_cr_plus_lf() {
        // A CRLF should count as one CRLF, not as one CR + one LF.
        assert_eq!(detect(b"x\r\ny"), Eol::CrLf);
    }

    #[test]
    fn bytes_round_trip() {
        assert_eq!(Eol::Lf.bytes(), b"\n");
        assert_eq!(Eol::CrLf.bytes(), b"\r\n");
        assert_eq!(Eol::Cr.bytes(), b"\r");
        assert_eq!(Eol::Mixed.bytes(), b"\n");
    }

    #[test]
    fn detection_is_bounded_to_64k() {
        // Construct 64 KiB of CRLF content followed by 1 KiB of CR-only
        // content. Detection should report CrLf because the tail is past
        // the 64 KiB cap.
        let mut buf = Vec::with_capacity(65_536 + 1024);
        while buf.len() < 65_536 {
            buf.extend_from_slice(b"line\r\n");
        }
        buf.truncate(65_536);
        buf.extend_from_slice(b"abc\rdef\rghi\r");
        assert_eq!(detect(&buf), Eol::CrLf);
    }

    #[test]
    fn long_label_matches_notepad_plus_plus_status_bar_form() {
        // The status bar reads `long_label` directly; pinning the
        // exact strings here keeps a future label tweak from
        // silently breaking the screenshot-driven user contract.
        assert_eq!(Eol::Lf.long_label(), "Unix (LF)");
        assert_eq!(Eol::CrLf.long_label(), "Windows (CR LF)");
        assert_eq!(Eol::Cr.long_label(), "Macintosh (CR)");
        assert_eq!(Eol::Mixed.long_label(), "Mixed");
    }
}
