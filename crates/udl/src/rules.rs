//! Parsed rule types for the compact `NN`-prefixed encoding
//! Notepad++ uses inside the UDL `Comments` and `Delimiters`
//! keyword lists.
//!
//! Phase 4.6 m1c-1: **rule parsing only** — this module converts
//! the raw strings the m1a parser stores verbatim on
//! [`crate::UdlKeywordLists::comments`] and
//! [`crate::UdlKeywordLists::delimiters`] into structured
//! [`CommentRules`] and [`DelimiterRules`] types. The actual
//! tokeniser walk that consumes these rules lands in m1c-2; the
//! `SCLEX_CONTAINER` / `SCN_STYLENEEDED` Scintilla wiring lands
//! in m1c-3.
//!
//! # Encoding recap
//!
//! Both strings are whitespace-separated tokens of the shape
//! `NN<content>` where `NN` is a 2-digit index (`00`..=`23`) and
//! `<content>` is one of:
//!
//! - **Empty** — no content for that slot (e.g. markdown's
//!   `01` in `Comments` means "no line-comment continuation
//!   character").
//! - **A literal byte sequence** — e.g. `00#` (line-comment
//!   marker is `#`), `03<!--` (block-comment opener is `<!--`).
//! - **`((EOL))`** — "close at end of line" — the entire
//!   `((EOL))` seven-byte sequence is one atom.
//! - **`((EOL <chars>))`** — "close at end of line unless
//!   `<chars>` follows" — used for triple-backtick fenced code
//!   blocks where the closer can wrap and consume more content
//!   until a backtick appears. **The space between `EOL` and
//!   `<chars>` is INSIDE the atom**, which is what makes naive
//!   `split_whitespace` on the raw string incorrect.
//!
//! # Index conventions
//!
//! - **Comments** (5 indices): `00` = line-open, `01` =
//!   line-continue-char, `02` = line-close (typically `((EOL))`),
//!   `03` = block-open, `04` = block-close.
//! - **Delimiters** (8 delimiters × 3 slots = 24 indices):
//!   delimiter *k* (0..=7) uses `3k` for open, `3k+1` for the
//!   escape character, `3k+2` for close. Multiple entries
//!   sharing the same index are alternative sequences (e.g.
//!   markdown's `00![ 00[` means the delimiter-0 opener can be
//!   either `![` or `[`).

/// Maximum number of alternative `open`/`close` sequences the
/// parser accepts per delimiter slot. Excess alternatives are
/// dropped with a `tracing::warn!`.
///
/// **`DoS` defence.** Without a cap, a hostile UDL could pack
/// tens of thousands of alternatives at a single delimiter
/// index within m1a's 256 KiB file-size cap (e.g.
/// `"00a 00a 00a ..."` at ~5 bytes/token → ~50K alternatives).
/// The m1c-2 tokeniser probes each alternative at every byte
/// position when nothing matches, so unbounded alternatives
/// become an `O(N × M)` per-buffer cost with `M` attacker-
/// controlled — freezing the UI thread when m1c-3 wires this
/// into `SCN_STYLENEEDED` (which fires synchronously on every
/// redraw / scroll).
///
/// **32** is generous relative to any real-world UDL —
/// Edditoria's markdown-plus-plus tops out at 3 alternatives
/// per slot; the whole notepad-plus-plus/userDefinedLanguages
/// community collection stays under 8. A malformed / hostile
/// UDL hitting this cap is a diagnostic-worthy event, not a
/// mistake we should silently accept.
pub const MAX_ALTERNATIVES_PER_SLOT: usize = 32;

/// Maximum byte length of a single [`Sequence::Literal`] value
/// (comment marker / block open-close / delimiter open-escape-
/// close). Excess-length literals are dropped with a
/// `tracing::warn!`.
///
/// **`DoS` defence.** The m1c-2 tokeniser scans for close
/// sequences by re-running `starts_with(literal.as_bytes())` at
/// every byte position of a delimiter body. A 100 KiB literal
/// close that never matches would drive the scan toward
/// `O(N² × L)` on documents containing many opener occurrences.
///
/// **64** is generous relative to the longest sequences the
/// markdown fixture uses (4 bytes for `<!--` / `-->` / ```` ``` ````
/// / `~~~`); N++'s own community UDLs stay under 16.
pub const MAX_LITERAL_BYTES: usize = 64;

/// A single content atom parsed from an `NN<content>` token.
/// The parser's own building block — not part of the public API
/// (both [`CommentRules`] and [`DelimiterRules`] surface a more
/// domain-appropriate shape).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Sequence {
    /// Empty — the slot has no entry. Semantically distinct from
    /// [`Sequence::Literal`] with an empty string; the parser
    /// preserves the distinction so an editor round-trip
    /// (deferred to m3) can emit the same shape it read.
    #[default]
    Empty,
    /// Literal byte sequence matched verbatim in the buffer.
    Literal(String),
    /// `((EOL))` — "close at end of line." No except-list.
    EndOfLine,
    /// `((EOL <chars>))` — "close at end of line unless one of
    /// `<chars>` follows." The tokeniser (m1c-2) treats this as
    /// "close when EOL and the character after the EOL doesn't
    /// belong to `<chars>`" — a state-carrying rule.
    EndOfLineExcept(String),
}

impl Sequence {
    /// Convenience — is this the empty variant?
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }
}

/// Parsed shape of the UDL `Comments` keyword-list encoding.
///
/// Reflects the 5-index layout `00..=04` documented at the
/// module-level. Every field is optional — a UDL may declare
/// only line comments (`00`/`01`/`02`), only block comments
/// (`03`/`04`), or both.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommentRules {
    /// Line-comment opener alternatives (index `00`) — the byte
    /// sequences that start a line comment. Multiple entries at
    /// index `00` accumulate rather than overwrite: real UDL
    /// v2.0 files (e.g. Luis Pisco's Cisco IOS UDL declares
    /// `"00! 00remark"` for two line-comment markers) expect
    /// both markers to fire, and last-wins would silently lose
    /// all but the trailing one. Empty vec = no line comment
    /// declared. Bounded by [`MAX_ALTERNATIVES_PER_SLOT`], same
    /// `DoS`-defence rationale as delimiter opener alternatives.
    pub line_open: Vec<Sequence>,
    /// Line-comment continuation character (index `01`) — for
    /// languages where a backslash at line end continues the
    /// comment onto the next line. Empty for most languages;
    /// non-empty for C's `//` continuation (rarely used) and a
    /// few others.
    pub line_continue: Sequence,
    /// Line-comment terminator (index `02`) — typically
    /// [`Sequence::EndOfLine`] for `//`-style comments.
    pub line_close: Sequence,
    /// Block-comment opener (index `03`) — e.g. `<!--` for
    /// markdown/HTML, `/*` for C-family, `{-` for Haskell.
    pub block_open: Sequence,
    /// Block-comment closer (index `04`) — the matching pair to
    /// `block_open`.
    pub block_close: Sequence,
}

/// Parsed shape of the UDL `Delimiters` keyword-list encoding.
/// Fixed-size array of 8 delimiter rules (the maximum N++
/// allows); rules that the UDL doesn't populate remain
/// [`DelimiterRule::default`] with empty open/close vectors.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DelimiterRules {
    /// Fixed 8-element table, indexed by delimiter number
    /// `0..=7`.
    pub rules: [DelimiterRule; 8],
}

/// One delimiter pair: opening sequence(s), escape character,
/// closing sequence(s).
///
/// A UDL may declare multiple `open` and multiple `close`
/// sequences for the same delimiter — the tokeniser (m1c-2)
/// treats them as alternatives. Markdown's delimiter 0 uses
/// two openers (`![` and `[`) so image links and normal links
/// share the same close-bracket family.
///
/// `escape` is single-character in practice — every N++
/// fixture I inspected uses a single-byte escape. The
/// [`Sequence`] type supports the general case for
/// forward compatibility but the m1c-2 tokeniser only honors
/// [`Sequence::Literal`] of length 1.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DelimiterRule {
    /// Opening sequences — the tokeniser matches ANY of these.
    /// Empty vec = "no opener" = "delimiter unused."
    pub open: Vec<Sequence>,
    /// Escape sequence — a byte that escapes the next character
    /// inside the delimited span. Typically `\`.
    pub escape: Sequence,
    /// Closing sequences — the tokeniser matches ANY of these.
    pub close: Vec<Sequence>,
}

impl CommentRules {
    /// Parse the `Comments` keyword-list encoding into
    /// structured [`CommentRules`]. Unknown / out-of-range
    /// indices are logged at warn and dropped rather than
    /// erroring — matches the tolerant-parsing discipline the
    /// rest of the crate follows.
    ///
    /// Tokens past index `04` are treated as extraneous and
    /// warn-skipped. Multiple tokens at index `00` accumulate
    /// (see [`Self::line_open`] — Cisco IOS's `"00! 00remark"`
    /// is a real example that would silently lose one marker
    /// under last-wins semantics). Multiple tokens at the other
    /// indices still last-wins — no real-world UDL declares
    /// multiple line-continuations or multiple block-openers.
    ///
    /// [`Sequence::Literal`] values exceeding
    /// [`MAX_LITERAL_BYTES`] are collapsed to [`Sequence::Empty`]
    /// with a `tracing::warn!` — see the cap's docstring for
    /// the `DoS` rationale.
    #[must_use]
    pub fn parse(encoded: &str) -> Self {
        let mut rules = Self::default();
        for token in tokenise_udl_encoding(encoded) {
            let content = cap_literal(token.content);
            match token.index {
                0 => push_bounded(&mut rules.line_open, content, "line_open", 0),
                1 => rules.line_continue = content,
                2 => rules.line_close = content,
                3 => rules.block_open = content,
                4 => rules.block_close = content,
                other => {
                    tracing::warn!(
                        index = other,
                        "UDL Comments token index out of range 0..=4; skipped"
                    );
                }
            }
        }
        rules
    }
}

impl DelimiterRules {
    /// Parse the `Delimiters` keyword-list encoding into
    /// structured [`DelimiterRules`]. Index `NN` decomposes as
    /// delimiter `NN / 3` (0..=7), slot `NN % 3` (0=open, 1=
    /// escape, 2=close). Indices past `23` are logged at warn
    /// and dropped.
    ///
    /// Empty-content entries (e.g. `21 22 23` at the tail of
    /// the markdown fixture, meaning "delimiter 7 is unused")
    /// are recorded as [`Sequence::Empty`] — the tokeniser
    /// treats them as no-match. This preserves the emit-back
    /// shape a future round-tripping editor (m3) will want.
    #[must_use]
    pub fn parse(encoded: &str) -> Self {
        let mut rules = Self::default();
        for token in tokenise_udl_encoding(encoded) {
            let delim = usize::from(token.index / 3);
            let slot = token.index % 3;
            if delim >= 8 {
                tracing::warn!(
                    index = token.index,
                    "UDL Delimiters token index out of range 0..=23; skipped"
                );
                continue;
            }
            let content = cap_literal(token.content);
            match slot {
                0 => push_bounded(&mut rules.rules[delim].open, content, "open", delim),
                1 => rules.rules[delim].escape = content,
                2 => push_bounded(&mut rules.rules[delim].close, content, "close", delim),
                _ => unreachable!("slot bounded by mod-3"),
            }
        }
        rules
    }
}

/// Downgrade a [`Sequence::Literal`] value whose content exceeds
/// [`MAX_LITERAL_BYTES`] to [`Sequence::Empty`], logging a warn.
/// See the cap's docstring for the `DoS` rationale.
fn cap_literal(seq: Sequence) -> Sequence {
    match seq {
        Sequence::Literal(l) if l.len() > MAX_LITERAL_BYTES => {
            tracing::warn!(
                len = l.len(),
                cap = MAX_LITERAL_BYTES,
                "UDL literal exceeds byte cap; collapsed to Empty \
                 (probable DoS-shaped input)"
            );
            Sequence::Empty
        }
        other => other,
    }
}

/// Push a non-empty sequence onto an alternatives vec, warning
/// and dropping the entry if the vec is already at
/// [`MAX_ALTERNATIVES_PER_SLOT`]. Empty entries are skipped
/// silently — they were preserved on `Sequence::Empty` variants
/// by earlier m1c-1 design (round-trip fidelity) but have no
/// runtime effect and shouldn't count toward the cap.
fn push_bounded(vec: &mut Vec<Sequence>, seq: Sequence, side: &'static str, delim: usize) {
    if seq.is_empty() {
        return;
    }
    if vec.len() >= MAX_ALTERNATIVES_PER_SLOT {
        tracing::warn!(
            side,
            delim,
            cap = MAX_ALTERNATIVES_PER_SLOT,
            "UDL delimiter alternative count exceeds cap; dropped \
             (probable DoS-shaped input)"
        );
        return;
    }
    vec.push(seq);
}

/// One parsed `NN<content>` token from the compact encoding.
/// Crate-private — only [`tokenise_udl_encoding`] produces it
/// and only [`CommentRules::parse`] / [`DelimiterRules::parse`]
/// consume it.
struct UdlToken {
    /// Numeric index parsed from the leading 2-digit prefix.
    /// `u8` is deliberately narrow — the domain is `0..=99` at
    /// the outside (`Comments` uses 0..=4, `Delimiters` uses
    /// 0..=23), so `u8` catches "did I forget to divide by 3"
    /// type errors at the parse site.
    index: u8,
    /// The content atom parsed from the token.
    content: Sequence,
}

/// Walk `s` and produce every `NN<content>` token in order.
/// Malformed tokens (non-digit prefix, half-open `((EOL X`
/// sequence) are logged at warn and dropped — the emit-what-
/// -we-can discipline again.
///
/// **`((EOL <chars>))` handling** is what makes this NOT a
/// `str::split_whitespace` one-liner. The `<chars>` field
/// contains an EMBEDDED space (the space between `EOL` and
/// the first except-char), which `split_whitespace` would
/// treat as a token boundary. We scan character-by-character,
/// switching into a `((EOL`-consuming mode when we see that
/// prefix and consuming through the matching `))`.
fn tokenise_udl_encoding(s: &str) -> Vec<UdlToken> {
    let bytes = s.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Every valid token starts with two ASCII digits.
        if i + 1 >= bytes.len() || !bytes[i].is_ascii_digit() || !bytes[i + 1].is_ascii_digit() {
            // Slice `s` directly (not push-by-byte via `u8 as char`)
            // so non-ASCII bytes in a hostile/hand-edited UDL render
            // as proper UTF-8 in the log rather than Latin-1
            // mojibake. The skip loop stops on an ASCII whitespace
            // byte — same boundary as UTF-8 — so `s[skip_start..i]`
            // is always a valid str slice at this point.
            let skip_start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            tracing::warn!(
                token = ?&s[skip_start..i],
                "UDL encoded token missing 2-digit prefix; skipped"
            );
            continue;
        }
        let index = (bytes[i] - b'0') * 10 + (bytes[i + 1] - b'0');
        i += 2;
        // Determine content shape.
        let content_start = i;
        if bytes.get(i..i + 5) == Some(b"((EOL") {
            // Skip past `((EOL`, then scan for closing `))`.
            // Everything between (including the embedded space if
            // present) is the except-list. Note this scan indexes
            // `bytes: &[u8]` (byte-slice), not `s: &str` (which
            // would require UTF-8 boundaries) — since the `))`
            // sentinel is ASCII, we don't need boundary-safe
            // slicing here. The `s[content_start..i]` slice below
            // (into `&str`) is only reached after `i` has advanced
            // past `))`, an ASCII sequence, so `i` sits on a
            // UTF-8 boundary at that point.
            i += 5;
            while i + 1 < bytes.len() && &bytes[i..i + 2] != b"))" {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            } else {
                // Unterminated `((EOL...` — emit anyway with
                // whatever content we consumed and warn. Include
                // the actual consumed tail (from `content_start`)
                // in the log so a real-world debugging session has
                // the offending bytes visible.
                tracing::warn!(
                    tail = ?&s[content_start..],
                    "UDL encoded token has unterminated ((EOL...; \
                     treating rest of string as content"
                );
                i = bytes.len();
            }
        } else {
            // Literal — read up to next whitespace.
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        let content_str = &s[content_start..i];
        let content = classify_content(content_str);
        tokens.push(UdlToken { index, content });
    }
    tokens
}

/// Turn a raw content substring into the corresponding
/// [`Sequence`] variant. Recognises `((EOL))` /
/// `((EOL <chars>))` explicitly; everything else is
/// [`Sequence::Literal`] (or [`Sequence::Empty`] when the
/// content is the empty string).
fn classify_content(s: &str) -> Sequence {
    if s.is_empty() {
        Sequence::Empty
    } else if s == "((EOL))" {
        Sequence::EndOfLine
    } else if let Some(inner) = s
        .strip_prefix("((EOL ")
        .and_then(|inner| inner.strip_suffix("))"))
    {
        Sequence::EndOfLineExcept(inner.to_owned())
    } else {
        Sequence::Literal(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Comment-rules parsing ---------------------------------

    #[test]
    fn markdown_comments_parse_to_structured_rules() {
        // Fixture: `00# 01 02((EOL)) 03<!-- 04-->`
        let rules = CommentRules::parse("00# 01 02((EOL)) 03<!-- 04-->");
        assert_eq!(rules.line_open, vec![Sequence::Literal("#".to_owned())]);
        assert_eq!(rules.line_continue, Sequence::Empty);
        assert_eq!(rules.line_close, Sequence::EndOfLine);
        assert_eq!(rules.block_open, Sequence::Literal("<!--".to_owned()));
        assert_eq!(rules.block_close, Sequence::Literal("-->".to_owned()));
    }

    #[test]
    fn c_style_comments_parse_correctly() {
        // Hypothetical C-family UDL: `//` line comment, `/* */`
        // block comment.
        let rules = CommentRules::parse("00// 01 02((EOL)) 03/* 04*/");
        assert_eq!(rules.line_open, vec![Sequence::Literal("//".to_owned())]);
        assert_eq!(rules.line_close, Sequence::EndOfLine);
        assert_eq!(rules.block_open, Sequence::Literal("/*".to_owned()));
        assert_eq!(rules.block_close, Sequence::Literal("*/".to_owned()));
    }

    #[test]
    fn out_of_range_index_in_comments_is_warned_not_errored() {
        // Only 00..=04 are valid; 05 must be dropped, other
        // entries must still populate.
        let rules = CommentRules::parse("00# 05IGNORED");
        assert_eq!(rules.line_open, vec![Sequence::Literal("#".to_owned())]);
        assert_eq!(rules.block_close, Sequence::Empty);
    }

    #[test]
    fn empty_comments_string_yields_default_rules() {
        let rules = CommentRules::parse("");
        assert_eq!(rules, CommentRules::default());
    }

    #[test]
    fn multiple_line_open_markers_accumulate_not_overwrite() {
        // Regression pin for the Cisco IOS UDL shape — real v2.0
        // files declare two line-comment markers via
        // `00! 00remark`. Pre-fix, `CommentRules::parse` used
        // `rules.line_open = content` (last-wins), silently
        // dropping the first marker and leaving `!`-started
        // lines unstyled. Confirm both survive in
        // input order.
        let rules = CommentRules::parse("00! 00remark 01 02((EOL))");
        assert_eq!(
            rules.line_open,
            vec![
                Sequence::Literal("!".to_owned()),
                Sequence::Literal("remark".to_owned()),
            ]
        );
    }

    // --- DoS caps ---------------------------------------------

    #[test]
    fn overlong_literal_collapses_to_empty() {
        // A `Literal` value exceeding MAX_LITERAL_BYTES is a
        // DoS-shaped input; the parser must collapse it to
        // `Empty` (not crash, not pass through unchanged).
        // `push_bounded` skips empty entries silently — no
        // marker is stored, `line_open` stays empty.
        let long = "a".repeat(MAX_LITERAL_BYTES + 1);
        let encoded = format!("00{long}");
        let rules = CommentRules::parse(&encoded);
        assert!(rules.line_open.is_empty());
    }

    #[test]
    fn max_alternatives_per_slot_enforced() {
        // Pack the parser with MAX_ALTERNATIVES_PER_SLOT + 5 open
        // sequences at delimiter slot 0. Only the first MAX
        // survive; the excess is dropped.
        let mut encoded = String::new();
        let count = MAX_ALTERNATIVES_PER_SLOT + 5;
        for i in 0..count {
            // Use unique-enough content per index so we can
            // distinguish which ones landed. Numeric suffix on
            // the letter `a`.
            let _ = std::fmt::Write::write_fmt(&mut encoded, format_args!("00a{i} "));
        }
        let rules = DelimiterRules::parse(&encoded);
        assert_eq!(rules.rules[0].open.len(), MAX_ALTERNATIVES_PER_SLOT);
    }

    // Sanity pin — the caps must not be so tight that a
    // realistic UDL trips them. Markdown fixture's largest slot
    // has 3 alternatives (delimiter 1 `open`), longest literal
    // is 4 bytes. Compile-time asserts because both operands are
    // consts, per clippy's assertions_on_constants lint.
    const _: () = assert!(MAX_ALTERNATIVES_PER_SLOT >= 8);
    const _: () = assert!(MAX_LITERAL_BYTES >= 16);

    // --- Delimiter-rules parsing -------------------------------

    #[test]
    fn markdown_delimiters_parse_to_structured_rules() {
        // Fixture:
        //   00![ 00[ 01\ 02] 02]
        //   03``` 03` 03~~~ 04\ 05``` 05((EOL `)) 05~~~
        //   06*** 07\ 08((EOL ***))
        //   09** 10\ 11((EOL **))
        //   12* 13\ 14((EOL *))
        //   15** 16\ 17((EOL **))
        //   18* 19\ 20((EOL *))
        //   21 22 23
        let rules = DelimiterRules::parse(
            "00![ 00[ 01\\ 02] 02] 03``` 03` 03~~~ 04\\ 05``` 05((EOL `)) 05~~~ \
             06*** 07\\ 08((EOL ***)) 09** 10\\ 11((EOL **)) 12* 13\\ 14((EOL *)) \
             15** 16\\ 17((EOL **)) 18* 19\\ 20((EOL *)) 21 22 23",
        );

        // Delimiter 0: `![`/`[` open, `\` escape, `]` close (twice).
        assert_eq!(
            rules.rules[0].open,
            vec![
                Sequence::Literal("![".to_owned()),
                Sequence::Literal("[".to_owned()),
            ]
        );
        assert_eq!(rules.rules[0].escape, Sequence::Literal("\\".to_owned()));
        assert_eq!(
            rules.rules[0].close,
            vec![
                Sequence::Literal("]".to_owned()),
                Sequence::Literal("]".to_owned()),
            ]
        );

        // Delimiter 1: triple-backtick / single-backtick / triple-
        // tilde openers; matching closers include the special
        // ((EOL `)) form.
        assert_eq!(
            rules.rules[1].open,
            vec![
                Sequence::Literal("```".to_owned()),
                Sequence::Literal("`".to_owned()),
                Sequence::Literal("~~~".to_owned()),
            ]
        );
        assert_eq!(
            rules.rules[1].close,
            vec![
                Sequence::Literal("```".to_owned()),
                Sequence::EndOfLineExcept("`".to_owned()),
                Sequence::Literal("~~~".to_owned()),
            ]
        );

        // Delimiter 2: `***` open, `\` escape, ((EOL ***)) close.
        assert_eq!(
            rules.rules[2].open,
            vec![Sequence::Literal("***".to_owned())]
        );
        assert_eq!(
            rules.rules[2].close,
            vec![Sequence::EndOfLineExcept("***".to_owned())]
        );

        // Delimiter 7 (the tail 21/22/23): all empty.
        assert!(rules.rules[7].open.is_empty());
        assert_eq!(rules.rules[7].escape, Sequence::Empty);
        assert!(rules.rules[7].close.is_empty());
    }

    #[test]
    fn out_of_range_delimiter_index_dropped() {
        let rules = DelimiterRules::parse("00X 99Y");
        assert_eq!(rules.rules[0].open, vec![Sequence::Literal("X".to_owned())]);
        // 99 is out of range (0..=23); nothing extra populated.
        assert_eq!(rules.rules[7], DelimiterRule::default());
    }

    // --- Encoding tokeniser edge cases -------------------------

    #[test]
    fn tokeniser_handles_embedded_space_in_eol_except() {
        // Regression pin: `((EOL X))` contains an embedded space
        // between EOL and X. Naive whitespace splitting would
        // break this into two tokens.
        let tokens = tokenise_udl_encoding("05((EOL `))");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].index, 5);
        assert_eq!(tokens[0].content, Sequence::EndOfLineExcept("`".to_owned()));
    }

    #[test]
    fn tokeniser_handles_multichar_eol_except() {
        // `((EOL ***))` — the except-list can be multiple bytes.
        let tokens = tokenise_udl_encoding("08((EOL ***))");
        assert_eq!(tokens.len(), 1);
        assert_eq!(
            tokens[0].content,
            Sequence::EndOfLineExcept("***".to_owned())
        );
    }

    #[test]
    fn tokeniser_handles_plain_eol() {
        let tokens = tokenise_udl_encoding("02((EOL))");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].content, Sequence::EndOfLine);
    }

    #[test]
    fn tokeniser_handles_empty_content() {
        let tokens = tokenise_udl_encoding("01");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].index, 1);
        assert_eq!(tokens[0].content, Sequence::Empty);
    }

    #[test]
    fn tokeniser_handles_multiple_tokens_with_same_index() {
        // Real-world case: markdown's `03``` 03` 03~~~` three
        // alternative openers at index 3.
        let tokens = tokenise_udl_encoding("03``` 03` 03~~~");
        assert_eq!(tokens.len(), 3);
        for token in &tokens {
            assert_eq!(token.index, 3);
        }
        assert_eq!(tokens[0].content, Sequence::Literal("```".to_owned()));
        assert_eq!(tokens[1].content, Sequence::Literal("`".to_owned()));
        assert_eq!(tokens[2].content, Sequence::Literal("~~~".to_owned()));
    }

    #[test]
    fn tokeniser_skips_malformed_prefix() {
        // A token missing the 2-digit prefix (e.g. from a hand-
        // edited UDL) is logged and dropped; the rest of the
        // stream still parses.
        let tokens = tokenise_udl_encoding("00A XX 01B");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].index, 0);
        assert_eq!(tokens[0].content, Sequence::Literal("A".to_owned()));
        assert_eq!(tokens[1].index, 1);
        assert_eq!(tokens[1].content, Sequence::Literal("B".to_owned()));
    }

    #[test]
    fn tokeniser_handles_unterminated_eol_form() {
        // Malformed `((EOL foo` with no closing `))`. We emit
        // what we can and move on — never panic.
        let tokens = tokenise_udl_encoding("05((EOL foo");
        // The unterminated form eats to end-of-string; we still
        // get one token out.
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].index, 5);
        // Whatever we emit for the unterminated form is
        // implementation-defined (either Literal("((EOL foo") or
        // an incomplete EndOfLineExcept); the important
        // invariant is that we didn't panic and produced ONE
        // token, not zero or many.
    }

    #[test]
    fn same_index_last_one_wins_in_non_accumulating_comment_slots() {
        // Non-`00` comment indices (line-continue, line-close,
        // block-open, block-close) still last-wins — no real UDL
        // declares multiples for those slots. Only `00`
        // (line_open) accumulates; that's covered by
        // `multiple_line_open_markers_accumulate_not_overwrite`.
        let rules = CommentRules::parse("03/* 03<!--");
        assert_eq!(rules.block_open, Sequence::Literal("<!--".to_owned()));
    }

    #[test]
    fn malformed_prefix_with_non_ascii_bytes_survives_utf8() {
        // Regression pin: the malformed-token skip loop must
        // slice the source string (which is valid UTF-8) rather
        // than push-byte-by-byte via `u8 as char` (which would
        // produce Latin-1 mojibake for non-ASCII code units).
        // Verifies that (a) parsing continues past the bad token
        // and (b) the following valid token still parses.
        let tokens = tokenise_udl_encoding("éxx 01B");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].index, 1);
        assert_eq!(tokens[0].content, Sequence::Literal("B".to_owned()));
    }

    #[test]
    fn sequence_is_empty_helper() {
        assert!(Sequence::Empty.is_empty());
        assert!(!Sequence::Literal("x".to_owned()).is_empty());
        assert!(!Sequence::EndOfLine.is_empty());
        assert!(!Sequence::EndOfLineExcept(String::new()).is_empty());
    }

    // --- Integration with the m1a UdlDefinition parser ---------

    #[test]
    fn full_markdown_udl_comment_rules_via_definition() {
        // End-to-end: parse the markdown UDL XML fixture, extract
        // the raw Comments string, feed it through
        // CommentRules::parse.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("preinstalled-udls")
            .join("markdown._preinstalled.udl.xml");
        let udl = crate::UdlDefinition::from_file(&path).expect("markdown UDL parses");
        let rules = CommentRules::parse(&udl.keyword_lists.comments);
        assert_eq!(rules.line_open, vec![Sequence::Literal("#".to_owned())]);
        assert_eq!(rules.line_close, Sequence::EndOfLine);
        assert_eq!(rules.block_open, Sequence::Literal("<!--".to_owned()));
        assert_eq!(rules.block_close, Sequence::Literal("-->".to_owned()));
    }

    #[test]
    fn full_markdown_udl_delimiter_rules_via_definition() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("preinstalled-udls")
            .join("markdown._preinstalled.udl.xml");
        let udl = crate::UdlDefinition::from_file(&path).expect("markdown UDL parses");
        let rules = DelimiterRules::parse(&udl.keyword_lists.delimiters);
        // Structural pin — 8 rules, delimiter 0 has two openers,
        // delimiter 1 has three (with the ((EOL `)) closer),
        // delimiter 7 empty.
        assert_eq!(rules.rules[0].open.len(), 2);
        assert_eq!(rules.rules[1].open.len(), 3);
        assert_eq!(rules.rules[1].close.len(), 3);
        assert!(rules.rules[1]
            .close
            .contains(&Sequence::EndOfLineExcept("`".to_owned())));
        assert!(rules.rules[7].open.is_empty());
    }
}
