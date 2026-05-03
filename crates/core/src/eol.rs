//! End-of-line style detection and bookkeeping.
//!
//! Notepad++ tracks the EOL style per buffer and preserves it on save
//! unless the user explicitly converts. Code++ matches that behaviour.
//!
//! See DESIGN.md §5.2.

use serde::{Deserialize, Serialize};

/// End-of-line style for a buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Eol::Lf => b"\n",
            Eol::CrLf => b"\r\n",
            Eol::Cr => b"\r",
            Eol::Mixed => b"\n",
        }
    }

    /// Human-readable label for the status bar.
    pub const fn label(self) -> &'static str {
        match self {
            Eol::Lf => "LF",
            Eol::CrLf => "CRLF",
            Eol::Cr => "CR",
            Eol::Mixed => "Mixed",
        }
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
}
