//! End-of-line style detection and bookkeeping.
//!
//! Notepad++ tracks the EOL style per buffer and preserves it on save
//! unless the user explicitly converts. Code++ matches that behaviour.
//!
//! See DESIGN.md §5.2.

use serde::{Deserialize, Serialize};

/// One kind of line ending: LF, CRLF or CR. [`Eol::ending`] gives the
/// one a buffer's new lines get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// `\n`.
    Lf,
    /// `\r\n`.
    CrLf,
    /// `\r`.
    Cr,
}

impl Ending {
    /// This ending's bytes.
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Ending::Lf => b"\n",
            Ending::CrLf => b"\r\n",
            Ending::Cr => b"\r",
        }
    }
}

impl From<Ending> for Eol {
    fn from(ending: Ending) -> Self {
        match ending {
            Ending::Lf => Eol::Lf,
            Ending::CrLf => Eol::CrLf,
            Ending::Cr => Eol::Cr,
        }
    }
}

/// End-of-line style for a buffer.
///
/// The default, `Lf`, is a session record's when it names none, a session
/// written before the field existed. It is not what a new buffer starts
/// with: that is the shell's `NEW_DOCUMENT_EOL`, CRLF on Windows.
///
/// Serialized as the human-readable [`label`](Self::label) string
/// (`"LF"` / `"CRLF"` / `"CR"`, and `"Mixed LF"` / `"Mixed CRLF"` /
/// `"Mixed CR"`) so `session.xml` is readable. An unrecognised label
/// deserializes to the default (`Lf`).
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
    /// The file ends its lines in more than one way, and its first line
    /// ends with this one.
    ///
    /// The status bar says "Mixed". Everything else treats the buffer as
    /// having that first ending, as Notepad++ takes a file's first line
    /// ending as its format (measured against 8.9.8.1): Enter inserts it,
    /// and `NPPM_GETBUFFERFORMAT` answers it. Saving writes every line as
    /// it is, as it does for any buffer.
    Mixed(Ending),
}

impl Eol {
    /// The ending a new line gets: this kind, or for [`Self::Mixed`] the
    /// first ending in the file.
    #[must_use]
    pub const fn ending(self) -> Ending {
        match self {
            Eol::Lf => Ending::Lf,
            Eol::CrLf => Ending::CrLf,
            Eol::Cr => Ending::Cr,
            Eol::Mixed(first) => first,
        }
    }

    /// The bytes of the ending a new line gets; see [`Self::ending`].
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        self.ending().bytes()
    }

    /// Whether the file ends its lines in more than one way.
    #[must_use]
    pub const fn is_mixed(self) -> bool {
        matches!(self, Eol::Mixed(_))
    }

    /// Short label, for `session.xml` and the log.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Eol::Lf => "LF",
            Eol::CrLf => "CRLF",
            Eol::Cr => "CR",
            Eol::Mixed(Ending::Lf) => "Mixed LF",
            Eol::Mixed(Ending::CrLf) => "Mixed CRLF",
            Eol::Mixed(Ending::Cr) => "Mixed CR",
        }
    }

    /// Long human-readable label, in the form Notepad++ shows in
    /// its status bar: "Windows (CR LF)", "Unix (LF)",
    /// "Macintosh (CR)". For [`Self::Mixed`] it is "Mixed", whatever
    /// the first ending, where Notepad++ shows the first ending's form:
    /// the file uses more than one ending, and the status bar says so
    /// rather than naming one of them.
    #[must_use]
    pub const fn long_label(self) -> &'static str {
        match self {
            Eol::Lf => "Unix (LF)",
            Eol::CrLf => "Windows (CR LF)",
            Eol::Cr => "Macintosh (CR)",
            Eol::Mixed(_) => "Mixed",
        }
    }

    /// Inverse of [`label`](Self::label). Unknown values default to
    /// `Lf` so a hand-edited session.xml doesn't crash the editor.
    /// A warning is logged via `tracing` so a corrupted session is
    /// diagnosable from the log.
    ///
    /// A bare `"Mixed"`, which a session holds when it was written before
    /// [`Self::Mixed`] carried its first ending, reads as
    /// `Mixed(Ending::Lf)`: LF is what such a buffer inserted then.
    pub fn from_label(s: &str) -> Eol {
        match s {
            "LF" => Eol::Lf,
            "CRLF" => Eol::CrLf,
            "CR" => Eol::Cr,
            "Mixed LF" | "Mixed" => Eol::Mixed(Ending::Lf),
            "Mixed CRLF" => Eol::Mixed(Ending::CrLf),
            "Mixed CR" => Eol::Mixed(Ending::Cr),
            other => {
                tracing::warn!(label = ?other, "unknown EOL label; defaulting to LF");
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

/// Detect the EOL style of `bytes` by counting line endings in the first
/// 64 KiB. Files with mixed endings (e.g., a CRLF file with stray bare
/// LFs) are reported as `Mixed`, carrying the kind of the first ending.
/// A CR on the window's last byte is read together with the byte after
/// it, so a CRLF split by the window's edge counts as one CRLF.
///
/// When the first 64 KiB holds no line ending, a file whose first line
/// is longer than that (minified code, a log line), the first ending
/// after it decides, as Notepad++ takes the first ending it finds
/// wherever it lies. `None` only for text with no line ending at all, an
/// empty or one-line file: it says nothing about its ending, and the
/// caller decides what such a buffer gets, as Notepad++ gives it the New
/// Document format.
///
/// The 64 KiB cap is to keep the count bounded for huge files; it is
/// extremely rare for the first 64 KiB to disagree with the rest of the
/// file on dominant EOL. The search past it stops at the first ending,
/// and reads the whole text only when there is none.
#[must_use]
pub fn detect(bytes: &[u8]) -> Option<Eol> {
    let scan = if bytes.len() > 65_536 {
        &bytes[..65_536]
    } else {
        bytes
    };

    let mut crlf = 0usize;
    let mut bare_lf = 0usize;
    let mut bare_cr = 0usize;
    let mut first = None;

    let mut i = 0;
    while i < scan.len() {
        match scan[i] {
            b'\r' => {
                // `bytes`, not `scan`: the LF of a CRLF whose CR is the
                // window's last byte lies just past it.
                if bytes.get(i + 1) == Some(&b'\n') {
                    crlf += 1;
                    first.get_or_insert(Ending::CrLf);
                    i += 2;
                    continue;
                }
                bare_cr += 1;
                first.get_or_insert(Ending::Cr);
            }
            b'\n' => {
                bare_lf += 1;
                first.get_or_insert(Ending::Lf);
            }
            _ => {}
        }
        i += 1;
    }

    let kinds = [crlf > 0, bare_lf > 0, bare_cr > 0]
        .iter()
        .filter(|present| **present)
        .count();

    let Some(first) = first else {
        return first_ending_after(bytes, scan.len());
    };
    Some(if kinds == 1 {
        first.into()
    } else {
        Eol::Mixed(first)
    })
}

/// The kind of the first line ending in `bytes` at or after `from`, or
/// `None` if there is none.
fn first_ending_after(bytes: &[u8], from: usize) -> Option<Eol> {
    let at = from
        + bytes[from..]
            .iter()
            .position(|&b| b == b'\r' || b == b'\n')?;
    Some(match (bytes[at], bytes.get(at + 1)) {
        (b'\r', Some(b'\n')) => Eol::CrLf,
        (b'\r', _) => Eol::Cr,
        _ => Eol::Lf,
    })
}

/// Rewrite every line ending in `text` to `eol`'s byte sequence.
///
/// The headless twin of Scintilla's `SCI_CONVERTEOLS`, for a buffer
/// whose document has not been materialised yet — a tab loaded in the
/// background holds its content only in `Tab::text` until it is first
/// activated, so a conversion requested before then has to rewrite the
/// string the document will later be filled from. Walks the text once:
/// a `\r\n` pair counts as one ending, never as a CR followed by an LF,
/// matching [`detect`]'s own counting.
///
/// [`Eol::Mixed`] returns the text unchanged. It names "the file ends
/// its lines in more than one way", which is no single ending to
/// convert to, and the caller (`Shell::set_buffer_eol_by_id`) refuses
/// it before reaching here.
#[must_use]
pub fn convert(text: &str, eol: Eol) -> String {
    if eol.is_mixed() {
        return text.to_owned();
    }
    let ending = match eol.ending() {
        Ending::CrLf => "\r\n",
        Ending::Cr => "\r",
        Ending::Lf => "\n",
    };
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 16);
    // Copy runs of ordinary text as `&str` slices rather than byte by
    // byte, so the result is built from valid UTF-8 pieces throughout —
    // `\r` and `\n` are ASCII, so a slice boundary at either never
    // splits a multi-byte sequence.
    let mut run_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\r' && b != b'\n' {
            i += 1;
            continue;
        }
        out.push_str(&text[run_start..i]);
        out.push_str(ending);
        if b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
        }
        i += 1;
        run_start = i;
    }
    out.push_str(&text[run_start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_normalises_mixed_endings_to_each_target() {
        let mixed = "a\r\nb\nc\rd";
        assert_eq!(convert(mixed, Eol::Lf), "a\nb\nc\nd");
        assert_eq!(convert(mixed, Eol::CrLf), "a\r\nb\r\nc\r\nd");
        assert_eq!(convert(mixed, Eol::Cr), "a\rb\rc\rd");
    }

    #[test]
    fn convert_treats_crlf_as_one_ending() {
        // The trap: a CRLF pair rewritten as two endings would double
        // every Windows line on a CR or LF conversion.
        assert_eq!(convert("x\r\ny\r\n", Eol::Lf), "x\ny\n");
        assert_eq!(convert("x\r\ny\r\n", Eol::Cr), "x\ry\r");
    }

    #[test]
    fn convert_is_identity_on_uniform_text_and_on_mixed_target() {
        assert_eq!(convert("a\nb\n", Eol::Lf), "a\nb\n");
        assert_eq!(convert("a\r\nb\r\n", Eol::CrLf), "a\r\nb\r\n");
        assert_eq!(convert("no endings", Eol::CrLf), "no endings");
        assert_eq!(convert("", Eol::Cr), "");
        assert_eq!(convert("a\r\nb\nc", Eol::Mixed(Ending::CrLf)), "a\r\nb\nc");
    }

    #[test]
    fn convert_keeps_multibyte_text_intact() {
        // A trailing CR at the end of input, and non-ASCII on both
        // sides of an ending.
        assert_eq!(convert("héllo\rwörld\r", Eol::CrLf), "héllo\r\nwörld\r\n");
    }

    #[test]
    fn empty_has_no_ending() {
        assert_eq!(detect(b""), None);
    }

    #[test]
    fn single_line_no_newline_has_no_ending() {
        assert_eq!(detect(b"hello world"), None);
    }

    #[test]
    fn the_first_line_ending_past_64k_decides_when_none_comes_before() {
        // A first line longer than the 64 KiB counted: the first ending
        // after it decides, as Notepad++ takes the first ending wherever it
        // lies, rather than the file reading as having none and taking the
        // new-document ending. Later endings are not counted.
        for (tail, eol) in [
            (&b"\r\nb\nc\n"[..], Eol::CrLf),
            (b"\nb\r\n", Eol::Lf),
            (b"\rb", Eol::Cr),
            (b"\r", Eol::Cr),
        ] {
            let mut buf = vec![b'x'; 70_000];
            buf.extend_from_slice(tail);
            assert_eq!(detect(&buf), Some(eol), "{tail:?}");
        }
        // Text longer than 64 KiB with no ending at all has none.
        assert_eq!(detect(&vec![b'x'; 70_000]), None);
        // An ending just inside the window is counted as before.
        let mut buf = vec![b'x'; 65_534];
        buf.extend_from_slice(b"\n");
        buf.extend(std::iter::repeat_n(b'x', 1000));
        assert_eq!(detect(&buf), Some(Eol::Lf));
    }

    #[test]
    fn pure_lf() {
        assert_eq!(detect(b"a\nb\nc\n"), Some(Eol::Lf));
    }

    #[test]
    fn pure_crlf() {
        assert_eq!(detect(b"a\r\nb\r\nc\r\n"), Some(Eol::CrLf));
    }

    #[test]
    fn pure_cr_classic_mac() {
        assert_eq!(detect(b"a\rb\rc\r"), Some(Eol::Cr));
    }

    #[test]
    fn mixed_lf_and_crlf() {
        assert_eq!(detect(b"a\nb\r\nc\n"), Some(Eol::Mixed(Ending::Lf)));
    }

    #[test]
    fn mixed_cr_and_lf() {
        assert_eq!(detect(b"a\rb\nc\r"), Some(Eol::Mixed(Ending::Cr)));
    }

    #[test]
    fn a_mixed_file_records_its_first_ending() {
        // The files the probe plugin opened in Notepad++ 8.9.8.1, which
        // took the first ending as each one's format.
        assert_eq!(detect(b"a\r\nb\nc\n"), Some(Eol::Mixed(Ending::CrLf)));
        assert_eq!(detect(b"a\nb\r\nc\r\n"), Some(Eol::Mixed(Ending::Lf)));
        assert_eq!(detect(b"a\rb\nc\r\n"), Some(Eol::Mixed(Ending::Cr)));
        // However few the first kind's lines are.
        let mut buf = b"head\r\n".to_vec();
        buf.extend(b"x\n".repeat(1000));
        assert_eq!(detect(&buf), Some(Eol::Mixed(Ending::CrLf)));
    }

    #[test]
    fn a_crlf_split_by_the_windows_edge_is_one_crlf() {
        // The CR is the window's last byte and its LF the first byte past
        // it. Read inside the window alone, the CR looked bare: a pure CRLF
        // file read as Macintosh (CR), or, with CRLF lines before it, as
        // Mixed.
        let mut buf = vec![b'x'; 65_535];
        buf.extend_from_slice(b"\r\n");
        buf.extend(b"y\r\n".repeat(10));
        assert_eq!(buf[65_535], b'\r');
        assert_eq!(detect(&buf), Some(Eol::CrLf));

        let mut buf = b"ab\r\n".repeat(100);
        buf.resize(65_535, b'x');
        buf.extend_from_slice(b"\r\n");
        buf.extend(b"y\r\n".repeat(10));
        assert_eq!(detect(&buf), Some(Eol::CrLf));

        // A bare CR there is still a CR, whether a byte follows it or the
        // text ends with the window.
        let mut buf = b"ab\r\n".repeat(100);
        buf.resize(65_535, b'x');
        buf.extend_from_slice(b"\ry\r\n");
        assert_eq!(detect(&buf), Some(Eol::Mixed(Ending::CrLf)));
        let mut buf = vec![b'x'; 65_535];
        buf.push(b'\r');
        assert_eq!(buf.len(), 65_536);
        assert_eq!(detect(&buf), Some(Eol::Cr));
    }

    #[test]
    fn crlf_not_counted_as_cr_plus_lf() {
        // A CRLF should count as one CRLF, not as one CR + one LF.
        assert_eq!(detect(b"x\r\ny"), Some(Eol::CrLf));
    }

    #[test]
    fn bytes_round_trip() {
        assert_eq!(Eol::Lf.bytes(), b"\n");
        assert_eq!(Eol::CrLf.bytes(), b"\r\n");
        assert_eq!(Eol::Cr.bytes(), b"\r");
        assert_eq!(Eol::Mixed(Ending::Lf).bytes(), b"\n");
        assert_eq!(Eol::Mixed(Ending::CrLf).bytes(), b"\r\n");
        assert_eq!(Eol::Mixed(Ending::Cr).bytes(), b"\r");
    }

    #[test]
    fn a_new_line_gets_the_first_ending_of_a_mixed_file() {
        for ending in [Ending::Lf, Ending::CrLf, Ending::Cr] {
            assert_eq!(Eol::Mixed(ending).ending(), ending);
            assert_eq!(Eol::from(ending).ending(), ending);
            assert!(Eol::Mixed(ending).is_mixed());
            assert!(!Eol::from(ending).is_mixed());
        }
    }

    #[test]
    fn labels_round_trip_and_a_bare_mixed_reads_as_lf_first() {
        for eol in [
            Eol::Lf,
            Eol::CrLf,
            Eol::Cr,
            Eol::Mixed(Ending::Lf),
            Eol::Mixed(Ending::CrLf),
            Eol::Mixed(Ending::Cr),
        ] {
            assert_eq!(Eol::from_label(eol.label()), eol, "{}", eol.label());
        }
        // What a session written before the first ending was kept holds:
        // such a buffer inserted LF.
        assert_eq!(Eol::from_label("Mixed"), Eol::Mixed(Ending::Lf));
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
        assert_eq!(detect(&buf), Some(Eol::CrLf));
    }

    #[test]
    fn long_label_matches_notepad_plus_plus_status_bar_form() {
        // The status bar reads `long_label` directly; pinning the
        // exact strings here keeps a future label tweak from
        // silently breaking the screenshot-driven user contract.
        assert_eq!(Eol::Lf.long_label(), "Unix (LF)");
        assert_eq!(Eol::CrLf.long_label(), "Windows (CR LF)");
        assert_eq!(Eol::Cr.long_label(), "Macintosh (CR)");
        assert_eq!(Eol::Mixed(Ending::Lf).long_label(), "Mixed");
        assert_eq!(Eol::Mixed(Ending::CrLf).long_label(), "Mixed");
        assert_eq!(Eol::Mixed(Ending::Cr).long_label(), "Mixed");
    }
}
