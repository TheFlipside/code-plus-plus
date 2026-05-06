//! Encoding detection and decode/encode helpers.
//!
//! Detection strategy (DESIGN.md §5.1):
//!   1. BOM-prefixed: UTF-8 / UTF-16 LE / UTF-16 BE / UTF-32 LE / UTF-32 BE.
//!   2. No BOM, strict UTF-8 decode of first 64 KiB succeeds and contains
//!      any non-ASCII byte → UTF-8.
//!   3. Pure ASCII → UTF-8 (lossless).
//!   4. UTF-16 without BOM heuristic: count zero bytes in even vs odd
//!      positions in first 8 KiB. Strong skew → UTF-16 LE / BE.
//!   5. Fallback to system default codepage via `encoding_rs`.

use encoding_rs::{Encoding as RsEncoding, UTF_16BE, UTF_16LE, UTF_8};
use serde::{Deserialize, Serialize};

/// The encoding of a buffer. Held alongside the decoded text so that
/// saving without explicit conversion writes the same bytes the file
/// arrived in.
///
/// Serialized as the human-readable [`label`](Self::label) string so
/// `session.xml` round-trips read like
/// `encoding="UTF-8"` / `encoding="windows-1252"`, never as serde's
/// default tagged-enum representation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum Encoding {
    /// UTF-8, with no BOM.
    #[default]
    Utf8,
    /// UTF-8 with BOM. On save the BOM is preserved.
    Utf8Bom,
    /// UTF-16 little-endian, with BOM.
    Utf16LeBom,
    /// UTF-16 big-endian, with BOM.
    Utf16BeBom,
    /// UTF-16 little-endian, no BOM (detected via zero-byte heuristic).
    Utf16Le,
    /// UTF-16 big-endian, no BOM.
    Utf16Be,
    /// A non-Unicode codepage identified by its WHATWG label, e.g.
    /// `windows-1252`, `shift_jis`, `gb18030`.
    Other(String),
}

impl Encoding {
    /// Human-readable label for the status bar and `session.xml`.
    pub fn label(&self) -> &str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 BOM",
            Encoding::Utf16LeBom => "UTF-16 LE BOM",
            Encoding::Utf16BeBom => "UTF-16 BE BOM",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Other(s) => s,
        }
    }

    /// Inverse of [`label`](Self::label). Unknown labels fall through
    /// to `Other`, preserving whatever the input was — so a hand-edited
    /// `session.xml` with an unrecognised encoding name doesn't crash
    /// the editor.
    pub fn from_label(s: &str) -> Encoding {
        match s {
            "UTF-8" => Encoding::Utf8,
            "UTF-8 BOM" => Encoding::Utf8Bom,
            "UTF-16 LE" => Encoding::Utf16Le,
            "UTF-16 LE BOM" => Encoding::Utf16LeBom,
            "UTF-16 BE" => Encoding::Utf16Be,
            "UTF-16 BE BOM" => Encoding::Utf16BeBom,
            other => Encoding::Other(other.to_owned()),
        }
    }
}

impl From<Encoding> for String {
    fn from(e: Encoding) -> Self {
        e.label().to_owned()
    }
}

impl From<String> for Encoding {
    fn from(s: String) -> Self {
        // Avoid an extra allocation when `Other` will own the string.
        match s.as_str() {
            "UTF-8" => Encoding::Utf8,
            "UTF-8 BOM" => Encoding::Utf8Bom,
            "UTF-16 LE" => Encoding::Utf16Le,
            "UTF-16 LE BOM" => Encoding::Utf16LeBom,
            "UTF-16 BE" => Encoding::Utf16Be,
            "UTF-16 BE BOM" => Encoding::Utf16BeBom,
            _ => Encoding::Other(s),
        }
    }
}

/// Detect the encoding of `bytes` and return `(encoding, body)` where
/// `body` is the slice with any BOM stripped — i.e. the bytes a decoder
/// should consume.
pub fn detect(bytes: &[u8]) -> (Encoding, &[u8]) {
    // 1. BOM-prefixed cases. Order matters: UTF-32 BOMs share a prefix
    //    with UTF-16 LE BOMs, so check UTF-32 first. Phase 2 doesn't
    //    distinguish UTF-32 from UTF-16-with-trailing-zeros — we surface
    //    them as `Other("utf-32-le"/"utf-32-be")` to make the file
    //    visible without losing the round-trip.
    if bytes.starts_with(b"\xFF\xFE\x00\x00") {
        return (Encoding::Other("utf-32-le".into()), &bytes[4..]);
    }
    if bytes.starts_with(b"\x00\x00\xFE\xFF") {
        return (Encoding::Other("utf-32-be".into()), &bytes[4..]);
    }
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        return (Encoding::Utf8Bom, &bytes[3..]);
    }
    if bytes.starts_with(b"\xFF\xFE") {
        return (Encoding::Utf16LeBom, &bytes[2..]);
    }
    if bytes.starts_with(b"\xFE\xFF") {
        return (Encoding::Utf16BeBom, &bytes[2..]);
    }

    // 2. UTF-16 no-BOM heuristic FIRST, before the UTF-8 check.
    //    Pure-ASCII UTF-16 LE bytes (`H\0e\0l\0l\0o\0`) are *also* valid
    //    UTF-8 (sequence of ASCII chars and NULs), so the UTF-8 path
    //    would happily accept them and mislabel the file. The zero-byte
    //    parity test catches that case unambiguously: real UTF-8 text
    //    has near-zero NULs, while ASCII-in-UTF-16 has ~50% zeros all on
    //    one parity.
    //
    //    Heuristic conditions, all of which must hold to declare UTF-16:
    //      - ≥16 bytes scanned (avoid spurious matches on short buffers)
    //      - ≥20% of scanned bytes are NUL (real text has very few)
    //      - ≥80% of those NULs are on one parity (LE = odd, BE = even)
    let heuristic_scan_len = bytes.len().min(8_192) & !1; // even length
    let mut zeros_even = 0usize;
    let mut zeros_odd = 0usize;
    for (i, &b) in bytes[..heuristic_scan_len].iter().enumerate() {
        if b == 0 {
            if i & 1 == 0 {
                zeros_even += 1;
            } else {
                zeros_odd += 1;
            }
        }
    }
    let total_zeros = zeros_even + zeros_odd;
    if heuristic_scan_len >= 16 && total_zeros * 5 >= heuristic_scan_len {
        if zeros_odd * 5 >= total_zeros * 4 {
            return (Encoding::Utf16Le, bytes);
        }
        if zeros_even * 5 >= total_zeros * 4 {
            return (Encoding::Utf16Be, bytes);
        }
    }

    // 3 & 4. Strict UTF-8 decode of the first 64 KiB. If valid (and the
    //        UTF-16 heuristic above didn't match), declare UTF-8. Pure
    //        ASCII still passes UTF-8 by design — UTF-8 is lossless for
    //        ASCII and there's no reason to label such a file otherwise.
    let scan_len = bytes.len().min(65_536);
    if std::str::from_utf8(&bytes[..scan_len]).is_ok() {
        // For files larger than the scan window, validate the rest
        // lazily on actual decode — at that point a partial-codepoint
        // error becomes a decode error and surfaces to the user.
        return (Encoding::Utf8, bytes);
    }

    // 5. Fallback. Windows-1252 is the most common legacy codepage on
    //    Windows-en machines and is the conventional fallback for "text
    //    that is not UTF-8 and has no BOM". Phase 2+ may choose a
    //    locale-aware default once we wire system codepage detection.
    (Encoding::Other("windows-1252".into()), bytes)
}

/// Decode `body` (BOM-stripped per `detect`) into a Rust `String` using
/// the supplied encoding.
///
/// Returns `EncodingError::Malformed` if any byte sequence is invalid
/// for the encoding (encoding_rs's strict mode), or `UnknownLabel` if
/// `Encoding::Other(label)` does not name an encoding `encoding_rs`
/// knows about. encoding_rs does not surface a byte offset for the
/// failing position; the caller surfaces a generic dialog and aborts
/// the open. A future opt-in lossy-decode path may relax this.
pub fn decode(body: &[u8], encoding: &Encoding) -> Result<String, EncodingError> {
    let rs_encoding: &'static RsEncoding = match encoding {
        Encoding::Utf8 | Encoding::Utf8Bom => UTF_8,
        Encoding::Utf16Le | Encoding::Utf16LeBom => UTF_16LE,
        Encoding::Utf16Be | Encoding::Utf16BeBom => UTF_16BE,
        Encoding::Other(label) => {
            RsEncoding::for_label(label.as_bytes()).ok_or_else(|| EncodingError::UnknownLabel {
                label: label.clone(),
            })?
        }
    };

    // Strict decode: any malformed sequence is reported, never replaced
    // silently. Notepad++ matches this behaviour.
    let (cow, _, had_errors) = rs_encoding.decode(body);
    if had_errors {
        return Err(EncodingError::Malformed {
            encoding: encoding.label().to_owned(),
        });
    }
    Ok(cow.into_owned())
}

/// Encode `text` to bytes using `encoding`, prepending a BOM if the
/// encoding requires one.
///
/// UTF-16 LE/BE are encoded manually because `encoding_rs` deliberately
/// does not support them as encode targets (the WHATWG Encoding Standard
/// it implements defines UTF-16 only for decoding; for encoding only
/// UTF-8 is in scope). Decoding still goes through `encoding_rs`.
pub fn encode(text: &str, encoding: &Encoding) -> Result<Vec<u8>, EncodingError> {
    match encoding {
        Encoding::Utf8 => Ok(text.as_bytes().to_vec()),
        Encoding::Utf8Bom => {
            let mut out = Vec::with_capacity(3 + text.len());
            out.extend_from_slice(b"\xEF\xBB\xBF");
            out.extend_from_slice(text.as_bytes());
            Ok(out)
        }
        Encoding::Utf16Le => Ok(encode_utf16(text, true, false)),
        Encoding::Utf16LeBom => Ok(encode_utf16(text, true, true)),
        Encoding::Utf16Be => Ok(encode_utf16(text, false, false)),
        Encoding::Utf16BeBom => Ok(encode_utf16(text, false, true)),
        Encoding::Other(label) => {
            let rs_encoding = RsEncoding::for_label(label.as_bytes()).ok_or_else(|| {
                EncodingError::UnknownLabel {
                    label: label.clone(),
                }
            })?;
            let (encoded, _, had_errors) = rs_encoding.encode(text);
            if had_errors {
                return Err(EncodingError::UnencodableChar {
                    encoding: label.clone(),
                });
            }
            Ok(encoded.into_owned())
        }
    }
}

/// Manual UTF-16 encoder. Produces little-endian when `le == true`,
/// big-endian otherwise; prepends the matching BOM when `with_bom`.
fn encode_utf16(text: &str, le: bool, with_bom: bool) -> Vec<u8> {
    let units: Vec<u16> = text.encode_utf16().collect();
    // Pre-size with checked arithmetic. A wrapping `units.len() * 2`
    // followed by a short allocation would invite a heap-overflow on
    // any future 32-bit port (~2 GB of text overflows `usize::MAX`
    // there); on 64-bit the bound is unreachable in practice but the
    // checked variant is free to write. On overflow we fall back to
    // a zero-capacity Vec — `extend_from_slice` will then grow the
    // allocation normally and the OOM (if any) surfaces in the
    // allocator, not as silent corruption.
    let cap = units
        .len()
        .checked_mul(2)
        .and_then(|n| n.checked_add(if with_bom { 2 } else { 0 }))
        .unwrap_or(0);
    let mut out = Vec::with_capacity(cap);
    if with_bom {
        out.extend_from_slice(if le { b"\xFF\xFE" } else { b"\xFE\xFF" });
    }
    for u in units {
        let bytes = if le { u.to_le_bytes() } else { u.to_be_bytes() };
        out.extend_from_slice(&bytes);
    }
    out
}

/// Encoding-related errors surfaced to the UI.
#[derive(Debug, PartialEq, Eq)]
pub enum EncodingError {
    /// The file contains bytes that are not valid in the declared
    /// encoding (e.g. a stray 0xC0 in UTF-8).
    Malformed { encoding: String },
    /// The text contains characters that the chosen encoding cannot
    /// represent (e.g. trying to write `é` as ASCII).
    UnencodableChar { encoding: String },
    /// The encoding label (`Encoding::Other`) is not known to
    /// `encoding_rs`.
    UnknownLabel { label: String },
}

impl std::fmt::Display for EncodingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodingError::Malformed { encoding } => {
                write!(f, "file contains malformed {encoding} data")
            }
            EncodingError::UnencodableChar { encoding } => {
                write!(
                    f,
                    "text contains characters that {encoding} cannot represent"
                )
            }
            EncodingError::UnknownLabel { label } => {
                write!(f, "unknown encoding label: {label}")
            }
        }
    }
}

impl std::error::Error for EncodingError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_utf8_bom() {
        let (enc, body) = detect(b"\xEF\xBB\xBFhello");
        assert_eq!(enc, Encoding::Utf8Bom);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn detects_utf16_le_bom() {
        let (enc, body) = detect(b"\xFF\xFEh\x00i\x00");
        assert_eq!(enc, Encoding::Utf16LeBom);
        assert_eq!(body, b"h\x00i\x00");
    }

    #[test]
    fn detects_utf16_be_bom() {
        let (enc, body) = detect(b"\xFE\xFF\x00h\x00i");
        assert_eq!(enc, Encoding::Utf16BeBom);
        assert_eq!(body, b"\x00h\x00i");
    }

    #[test]
    fn detects_utf32_le_bom() {
        let (enc, body) = detect(b"\xFF\xFE\x00\x00h\x00\x00\x00");
        assert_eq!(enc, Encoding::Other("utf-32-le".into()));
        assert_eq!(body, b"h\x00\x00\x00");
    }

    #[test]
    fn pure_ascii_is_utf8() {
        let (enc, body) = detect(b"hello world\n");
        assert_eq!(enc, Encoding::Utf8);
        assert_eq!(body, b"hello world\n");
    }

    #[test]
    fn valid_utf8_with_non_ascii_is_utf8() {
        // "café" in UTF-8.
        let (enc, body) = detect(b"caf\xC3\xA9");
        assert_eq!(enc, Encoding::Utf8);
        assert_eq!(body, b"caf\xC3\xA9");
    }

    #[test]
    fn detects_utf16_le_no_bom() {
        // ASCII "hello world\n" in UTF-16 LE without a BOM. Every odd
        // byte is zero.
        let bytes: Vec<u8> = "hello world\nhello world\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let (enc, _) = detect(&bytes);
        assert_eq!(enc, Encoding::Utf16Le);
    }

    #[test]
    fn detects_utf16_be_no_bom() {
        let bytes: Vec<u8> = "hello world\nhello world\n"
            .encode_utf16()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        let (enc, _) = detect(&bytes);
        assert_eq!(enc, Encoding::Utf16Be);
    }

    #[test]
    fn invalid_utf8_falls_back_to_codepage() {
        // Lone 0x80 is invalid as UTF-8 (continuation byte without
        // a leader). No BOM, no obvious UTF-16 pattern.
        let (enc, body) = detect(b"\x80\x81\x82");
        assert_eq!(enc, Encoding::Other("windows-1252".into()));
        assert_eq!(body, b"\x80\x81\x82");
    }

    #[test]
    fn empty_is_utf8() {
        let (enc, body) = detect(b"");
        assert_eq!(enc, Encoding::Utf8);
        assert_eq!(body, b"");
    }

    #[test]
    fn decode_utf8_round_trip() {
        let s = "Hello, 世界! 🎉";
        let bytes = encode(s, &Encoding::Utf8).unwrap();
        assert_eq!(bytes, s.as_bytes());
        let decoded = decode(&bytes, &Encoding::Utf8).unwrap();
        assert_eq!(decoded, s);
    }

    #[test]
    fn decode_utf8_bom_round_trip() {
        let s = "Hello";
        let bytes = encode(s, &Encoding::Utf8Bom).unwrap();
        assert_eq!(&bytes[..3], b"\xEF\xBB\xBF");
        // The body slice that decode() receives is BOM-stripped per
        // detect's contract, so feed it the stripped body.
        let (_, body) = detect(&bytes);
        let decoded = decode(body, &Encoding::Utf8Bom).unwrap();
        assert_eq!(decoded, s);
    }

    #[test]
    fn decode_utf16_le_round_trip() {
        let s = "Hello, 世界!";
        let bytes = encode(s, &Encoding::Utf16LeBom).unwrap();
        let (enc, body) = detect(&bytes);
        assert_eq!(enc, Encoding::Utf16LeBom);
        assert_eq!(decode(body, &Encoding::Utf16LeBom).unwrap(), s);
    }

    #[test]
    fn decode_malformed_utf8_errors() {
        let bad = b"\xC3\x28"; // C3 should be followed by a continuation
        let err = decode(bad, &Encoding::Utf8).unwrap_err();
        assert!(matches!(err, EncodingError::Malformed { .. }));
    }

    #[test]
    fn encode_unrepresentable_errors() {
        // Windows-1252 can't represent emoji.
        let err = encode("hi 🎉", &Encoding::Other("windows-1252".into())).unwrap_err();
        assert!(matches!(err, EncodingError::UnencodableChar { .. }));
    }

    #[test]
    fn unknown_encoding_label_errors_on_encode() {
        let err = encode("hi", &Encoding::Other("totally-fake-encoding".into())).unwrap_err();
        assert!(matches!(err, EncodingError::UnknownLabel { .. }));
    }

    #[test]
    fn unknown_encoding_label_errors_on_decode() {
        // Regression: decode used to silently fall back to Windows-1252
        // for unknown labels, corrupting data without warning.
        let err = decode(b"hello", &Encoding::Other("totally-fake-encoding".into())).unwrap_err();
        assert!(matches!(err, EncodingError::UnknownLabel { .. }));
    }

    #[test]
    fn detects_non_ascii_bmp_utf16() {
        // Greek alpha U+03B1 in UTF-16 LE: 0xB1 0x03. The first byte is
        // non-zero but the parity heuristic should still fire because
        // every second byte (the high byte for BMP) is zero.
        let s = "α α α α α α α α α α α α α α α α";
        let bytes: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let (enc, _) = detect(&bytes);
        assert_eq!(enc, Encoding::Utf16Le);
    }

    #[test]
    fn windows1252_round_trip() {
        // "café" — é is 0xE9 in Windows-1252.
        let s = "café";
        let bytes = encode(s, &Encoding::Other("windows-1252".into())).unwrap();
        assert_eq!(bytes, b"caf\xE9");
        let decoded = decode(&bytes, &Encoding::Other("windows-1252".into())).unwrap();
        assert_eq!(decoded, s);
    }
}
