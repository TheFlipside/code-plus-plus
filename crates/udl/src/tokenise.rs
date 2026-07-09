//! Container-lexer tokeniser — walks a byte buffer and emits
//! [`StyleEvent`]s per the rules parsed from a [`UdlDefinition`].
//!
//! Phase 4.6 m1c-2. Feeds m1c-3's `SCN_STYLENEEDED` handler in
//! `ui_win32`: Scintilla asks the host for styling on UDL
//! buffers, `ui_win32` calls [`Tokeniser::tokenise`] against the
//! requested range, walks the returned events and issues
//! `SCI_STARTSTYLING` + `SCI_SETSTYLING` per event.
//!
//! # Scope for this slice
//!
//! **Covered**: line comments, block comments, delimiter pairs
//! (including `((EOL))` and `((EOL <chars>))` closers treated
//! as end-of-line for m1c-2 — the except-list is honoured in a
//! later refinement), and Default fill for everything else.
//!
//! **Not covered yet** (each is its own follow-up commit —
//! adding them on top of the scaffold below is additive, none
//! of the below has to change):
//! - Keyword classes (`Keywords1..=8`) — the `case_ignored` /
//!   `prefix` flags on [`UdlDefinition`] make this its own
//!   sub-problem.
//! - Operators (`Operators1` / `Operators2`).
//! - Numbers (needs prefix / extras / suffix / range parsing).
//! - Delimiter escape characters — `DelimiterRule::escape` is
//!   ignored today; escapes inside strings render as ordinary
//!   delimited content.
//! - Full `((EOL <chars>))` context-sensitive closes — the
//!   tokeniser treats the except-list as "empty" for m1c-2, so
//!   e.g. markdown's triple-backtick fenced code block closes
//!   at the first `\n` even when the next line starts with a
//!   backtick.
//! - Case-insensitive literal matching for comments /
//!   delimiters. The `case_ignored` flag on
//!   [`crate::UdlSettings`] is documented as applying to every
//!   keyword list including delimiters, but this slice matches
//!   comment/delimiter openers/closers case-sensitively via
//!   byte-comparison. For every fixture we ship (markdown +
//!   likely third-party UDLs) the markers are ASCII symbols
//!   with no case dimension, so this doesn't currently mismatch
//!   any real UDL — but it would fail for a hypothetical UDL
//!   using letter-based markers with the flag on. Fixed
//!   alongside the keyword-class slice, where the same code
//!   path (case-folded byte comparison) is needed anyway.
//! - Folder markers.
//!
//! # Alternative-ordering discipline
//!
//! A `DelimiterRule` may list multiple `open` (or `close`)
//! [`Sequence`]s — the tokeniser matches ANY of them.
//! [`Tokeniser::new`] sorts each rule's alternatives by
//! **descending literal length** so a UDL declaring both `` ` ``
//! and ```` ``` ```` as openers matches the triple-backtick
//! (longer) first when the buffer has ```` ``` ````. Without
//! that sort the tokeniser would match the single-backtick and
//! misclassify the following two backticks as buffer content —
//! the same failure mode DESIGN.md §7.2 explicitly rejects
//! ("confirm the highlighting matches Notepad++'s rendering
//! byte-for-byte" for arbitrary community UDLs). Non-literal
//! variants (`EndOfLine` / `EndOfLineExcept`) sort as length 0
//! so they always come last — those close on newline, so an
//! earlier newline-match would preempt any literal-close-on-
//! newline case.
//!
//! # Overall shape
//!
//! Whole-buffer scan (not incremental yet). Scintilla will
//! typically ask for a viewport-sized range on
//! `SCN_STYLENEEDED`; m1c-3 wires that request against the
//! FULL document text for correctness and revisits performance
//! only if the resulting cost busts DESIGN.md §8's keystroke-
//! latency budget. Since m1c-2 is pure Rust with no Scintilla
//! calls, we can benchmark this independently before wiring it.
//!
//! # DoS-defence caps (see [`crate::MAX_ALTERNATIVES_PER_SLOT`]
//! and [`crate::MAX_LITERAL_BYTES`])
//!
//! The rule parser caps both the number of alternative
//! sequences per delimiter slot and the byte length of each
//! literal. Without these caps, a hostile UDL dropped in
//! `<config_dir>/userDefineLangs/` could pack tens of thousands
//! of alternatives at a single index within m1a's 256 KiB file
//! cap, and this tokeniser would probe each at every byte
//! position — an `O(N × M)` cost with `M` attacker-controlled
//! that would freeze the UI thread once m1c-3 dispatches
//! tokenisation from `SCN_STYLENEEDED` (which fires
//! synchronously on every scroll / redraw). The caps drop the
//! excess with a `tracing::warn!` at parse time so the
//! tokeniser only ever sees a bounded rule set.
//!
//! **m1c-3 must additionally add incremental / bounded-range
//! tokenisation** before wiring `SCN_STYLENEEDED` — even with
//! bounded rules, a full-buffer rescan on every scroll of a
//! large document would still miss DESIGN.md §8's keystroke-
//! latency budget. Documented as a hard blocker for that
//! commit.

use crate::rules::{CommentRules, DelimiterRules, Sequence};
use crate::UdlDefinition;

/// Style slot emitted by the tokeniser — one per named
/// `<WordsStyle>` slot in the UDL. Numeric discriminants match
/// Notepad++'s `SCE_USER_STYLE_*` container-lexer indices per
/// `PowerEditor/src/ScintillaComponent/UserDefineDialog.h` (the
/// discriminant table below cites the exact index against the
/// N++ public constant so m1c-3 can pass the discriminant value
/// straight to `SCI_SETSTYLING` without a translation table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UdlStyleSlot {
    /// `SCE_USER_STYLE_DEFAULT` (0) — unclassified text.
    Default = 0,
    /// `SCE_USER_STYLE_COMMENT` (1) — block comment span.
    Comment = 1,
    /// `SCE_USER_STYLE_COMMENTLINE` (2) — line comment span.
    CommentLine = 2,
    /// `SCE_USER_STYLE_NUMBER` (3) — numeric literals.
    Number = 3,
    /// `SCE_USER_STYLE_WORD1..=8` (4..=11) — keyword classes.
    Word1 = 4,
    /// See [`Self::Word1`].
    Word2 = 5,
    /// See [`Self::Word1`].
    Word3 = 6,
    /// See [`Self::Word1`].
    Word4 = 7,
    /// See [`Self::Word1`].
    Word5 = 8,
    /// See [`Self::Word1`].
    Word6 = 9,
    /// See [`Self::Word1`].
    Word7 = 10,
    /// See [`Self::Word1`].
    Word8 = 11,
    /// `SCE_USER_STYLE_OPERATOR` (12).
    Operator = 12,
    /// `SCE_USER_STYLE_FOLDER_IN_CODE1` (13).
    FolderInCode1 = 13,
    /// `SCE_USER_STYLE_FOLDER_IN_CODE2` (14).
    FolderInCode2 = 14,
    /// `SCE_USER_STYLE_FOLDER_IN_COMMENT` (15).
    FolderInComment = 15,
    /// `SCE_USER_STYLE_DELIMITER1..=8` (16..=23) — delimiter
    /// spans by index.
    Delimiter1 = 16,
    /// See [`Self::Delimiter1`].
    Delimiter2 = 17,
    /// See [`Self::Delimiter1`].
    Delimiter3 = 18,
    /// See [`Self::Delimiter1`].
    Delimiter4 = 19,
    /// See [`Self::Delimiter1`].
    Delimiter5 = 20,
    /// See [`Self::Delimiter1`].
    Delimiter6 = 21,
    /// See [`Self::Delimiter1`].
    Delimiter7 = 22,
    /// See [`Self::Delimiter1`].
    Delimiter8 = 23,
}

impl UdlStyleSlot {
    /// Map delimiter index `0..=7` to the corresponding
    /// `Delimiter1..=8` variant. Panics on out-of-range input —
    /// callers must bound-check.
    #[must_use]
    fn from_delim_idx(idx: usize) -> Self {
        match idx {
            0 => Self::Delimiter1,
            1 => Self::Delimiter2,
            2 => Self::Delimiter3,
            3 => Self::Delimiter4,
            4 => Self::Delimiter5,
            5 => Self::Delimiter6,
            6 => Self::Delimiter7,
            7 => Self::Delimiter8,
            _ => {
                unreachable!("delimiter index bounded to 0..=7 by DelimiterRules::rules array size")
            }
        }
    }
}

/// One `(byte-range, style)` pair emitted by the tokeniser.
/// `start` is inclusive, `end` is exclusive. `end > start`
/// invariant holds — the tokeniser never emits zero-width
/// events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleEvent {
    /// Inclusive start byte offset into the input buffer.
    pub start: usize,
    /// Exclusive end byte offset. `end > start` always.
    pub end: usize,
    /// Style slot for the range.
    pub slot: UdlStyleSlot,
}

/// Owning tokeniser — parses the UDL rules once at construction
/// so [`Self::tokenise`] doesn't re-do the compact-string parse
/// on every call.
///
/// Not `Clone` because the [`UdlDefinition`] reference is
/// borrowed; the intended lifetime is the m1c-3 handler
/// building a `Tokeniser` once per document activation and
/// keeping it alive for that document's lifetime.
#[derive(Debug)]
pub struct Tokeniser<'a> {
    /// Kept as a back-reference for future refinements (case-
    /// sensitivity flag, keyword lookup) that need access to
    /// the raw definition.
    #[allow(dead_code)]
    def: &'a UdlDefinition,
    comment: CommentRules,
    delims: DelimiterRules,
}

impl<'a> Tokeniser<'a> {
    /// Build a tokeniser from a parsed UDL. Parses the compact
    /// `Comments` / `Delimiters` encodings once here so each
    /// [`Self::tokenise`] call is a pure state-machine walk.
    /// Also sorts each delimiter rule's `open` / `close`
    /// alternatives by descending literal length so longest-
    /// match wins — see the module-level "Alternative-ordering
    /// discipline" section for why.
    #[must_use]
    pub fn new(def: &'a UdlDefinition) -> Self {
        let mut delims = DelimiterRules::parse(&def.keyword_lists.delimiters);
        for rule in &mut delims.rules {
            sort_by_descending_literal_len(&mut rule.open);
            sort_by_descending_literal_len(&mut rule.close);
        }
        Self {
            def,
            comment: CommentRules::parse(&def.keyword_lists.comments),
            delims,
        }
    }

    /// Tokenise the whole `text` buffer.
    ///
    /// Returns events in position order. Every byte in
    /// `0..text.len()` is covered by exactly one event — the
    /// returned vec's `.iter().all(|e| e.end > e.start)`
    /// invariant plus adjacency (`events[k].end ==
    /// events[k+1].start`) is what m1c-3's `SCI_SETSTYLING`
    /// pass relies on to paint the whole buffer.
    #[must_use]
    pub fn tokenise(&self, text: &[u8]) -> Vec<StyleEvent> {
        let mut events = Vec::new();
        let mut i = 0;
        while i < text.len() {
            // Try each opener in priority order. First hit wins
            // — matches N++'s own precedence (line comments
            // outrank block, both outrank delimiters).
            if let Some(len) = self.match_line_comment(text, i) {
                push_event(&mut events, i, i + len, UdlStyleSlot::CommentLine);
                i += len;
                continue;
            }
            if let Some(len) = self.match_block_comment(text, i) {
                push_event(&mut events, i, i + len, UdlStyleSlot::Comment);
                i += len;
                continue;
            }
            if let Some((delim_idx, len)) = self.match_delimiter(text, i) {
                let slot = UdlStyleSlot::from_delim_idx(delim_idx);
                push_event(&mut events, i, i + len, slot);
                i += len;
                continue;
            }
            // Default fill — advance one byte and either extend
            // the trailing Default event or start a new one.
            let start = i;
            i += 1;
            push_default(&mut events, start, i);
        }
        events
    }

    /// If a line-comment opener matches at `pos`, return the
    /// total span length (from opener start to line end). Else
    /// `None`. Line-comment span always terminates at the next
    /// `\n` (or end-of-buffer), regardless of the
    /// [`CommentRules::line_close`] variant — [`Sequence::EndOfLine`]
    /// is the only value markdown / C / SQL / every other
    /// mainstream UDL actually uses, and honouring a literal
    /// `line_close` sequence would need a lookahead scan the
    /// existing fixtures never exercise.
    fn match_line_comment(&self, text: &[u8], pos: usize) -> Option<usize> {
        let open = literal_str(&self.comment.line_open)?;
        if !text[pos..].starts_with(open.as_bytes()) {
            return None;
        }
        // Walk to the next `\n` or end of buffer. The event
        // includes the newline byte if present so adjacent events
        // stay tight (the next line's event starts at the byte
        // after the newline).
        let mut i = pos + open.len();
        while i < text.len() && text[i] != b'\n' {
            i += 1;
        }
        if i < text.len() {
            i += 1; // include the newline byte
        }
        Some(i - pos)
    }

    /// If a block-comment opener matches at `pos`, return the
    /// total span length up to and including the block-close
    /// literal (or to end-of-buffer if no closer is found).
    fn match_block_comment(&self, text: &[u8], pos: usize) -> Option<usize> {
        let open = literal_str(&self.comment.block_open)?;
        let close = literal_str(&self.comment.block_close)?;
        if !text[pos..].starts_with(open.as_bytes()) {
            return None;
        }
        let mut i = pos + open.len();
        while i + close.len() <= text.len() && !text[i..].starts_with(close.as_bytes()) {
            i += 1;
        }
        if i + close.len() <= text.len() {
            i += close.len();
        } else {
            // Unterminated block comment — swallow the rest of
            // the buffer, matching N++'s permissive behaviour.
            i = text.len();
        }
        Some(i - pos)
    }

    /// If any delimiter rule's opener matches at `pos`, return
    /// `Some((delim_idx, span_length))`. Delimiter indices are
    /// tried in order 0..=7, so a UDL with overlapping openers
    /// declares its priority by ordering.
    fn match_delimiter(&self, text: &[u8], pos: usize) -> Option<(usize, usize)> {
        for (idx, rule) in self.delims.rules.iter().enumerate() {
            for open_seq in &rule.open {
                let Some(open) = literal_str(open_seq) else {
                    continue;
                };
                if !text[pos..].starts_with(open.as_bytes()) {
                    continue;
                }
                let span_end = Self::scan_delim_close(text, pos + open.len(), &rule.close);
                return Some((idx, span_end - pos));
            }
        }
        None
    }

    /// Given a delimiter's close-sequence list, scan from
    /// `body_start` until any close matches, returning the byte
    /// index of the character AFTER the close. `EndOfLine` /
    /// `EndOfLineExcept` variants close at the next `\n` (the
    /// except-list is a m1c-2 known-simplification).
    ///
    /// Free-standing (no `self`) because the scan reads only its
    /// arguments — keeps it independently unit-testable and
    /// avoids an unused-self clippy pedantic.
    fn scan_delim_close(text: &[u8], body_start: usize, closes: &[Sequence]) -> usize {
        let mut i = body_start;
        while i < text.len() {
            for close in closes {
                match close {
                    Sequence::Literal(lit) if text[i..].starts_with(lit.as_bytes()) => {
                        return i + lit.len();
                    }
                    Sequence::EndOfLine | Sequence::EndOfLineExcept(_) if text[i] == b'\n' => {
                        // Include the newline byte so adjacent
                        // events tile the buffer.
                        return i + 1;
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        text.len()
    }
}

/// Push a `[start, end)` event, coalescing with the trailing
/// event when it has the same slot AND is directly adjacent.
/// The tokeniser emits ranges naturally in position order, so
/// coalescing keeps the event count small without extra passes.
fn push_event(events: &mut Vec<StyleEvent>, start: usize, end: usize, slot: UdlStyleSlot) {
    // `assert!` (not `debug_assert!`) because m1c-3's downstream
    // FFI to `SCI_SETSTYLING` computes `end - start` as a length
    // argument — a zero-width or inverted event would silently
    // underflow `usize` in release mode and pass a huge length
    // to Scintilla. Fires exactly once per event; overall
    // tokenise cost is dominated by the byte scans, not this
    // per-event bound check.
    assert!(end > start, "zero-width events not permitted");
    if let Some(last) = events.last_mut() {
        if last.slot == slot && last.end == start {
            last.end = end;
            return;
        }
    }
    events.push(StyleEvent { start, end, slot });
}

/// Specialised push for the per-byte Default fill hot path —
/// same coalescing logic but skips the slot equality branch
/// (Default is what most bytes end up as).
fn push_default(events: &mut Vec<StyleEvent>, start: usize, end: usize) {
    push_event(events, start, end, UdlStyleSlot::Default);
}

/// Sort a `Vec<Sequence>` by descending literal byte length,
/// using a stable sort so equal-length ties preserve the input
/// order. Non-`Literal` variants (`EndOfLine` /
/// `EndOfLineExcept`) sort as length 0 — they always go last
/// among literals but never displace them.
fn sort_by_descending_literal_len(seqs: &mut [Sequence]) {
    // `usize::MAX - len` reverses the sort direction while
    // keeping `sort_by_key` (clippy prefers key-fn over the
    // general cmp-fn for a simple comparator).
    seqs.sort_by_key(|s| usize::MAX - literal_len(s));
}

/// Byte length of a [`Sequence::Literal`]; 0 for every other
/// variant. Used only by [`sort_by_descending_literal_len`].
fn literal_len(seq: &Sequence) -> usize {
    if let Sequence::Literal(l) = seq {
        l.len()
    } else {
        0
    }
}

/// Return the inner `&str` for a [`Sequence::Literal`]; every
/// other variant returns `None`. This is what the tokeniser
/// falls back to when matching against buffer text — the
/// `EndOfLine` / `EndOfLineExcept` variants are meaningful only
/// as delimiter *closers* (handled inside
/// [`Tokeniser::scan_delim_close`]), so treating them as "no
/// match" at the opener check is correct.
fn literal_str(seq: &Sequence) -> Option<&str> {
    if let Sequence::Literal(l) = seq {
        Some(l.as_str())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `UdlDefinition` with just `comments` and
    /// `delimiters` populated — the tokeniser reads only those
    /// two keyword-list strings today so leaving the rest at
    /// their defaults is fine for these tests.
    fn synth_udl(comments: &str, delimiters: &str) -> UdlDefinition {
        let kw = crate::UdlKeywordLists {
            comments: comments.to_owned(),
            delimiters: delimiters.to_owned(),
            ..crate::UdlKeywordLists::default()
        };
        UdlDefinition {
            name: "test".to_owned(),
            extensions: vec!["t".to_owned()],
            udl_version: "2.1".to_owned(),
            dark_mode_theme: false,
            settings: crate::UdlSettings {
                case_ignored: true,
                allow_fold_of_comments: false,
                fold_compact: false,
                force_pure_lc: 0,
                decimal_separator: 0,
            },
            prefix: [false; 8],
            keyword_lists: kw,
            styles: Vec::new(),
            source_path: None,
        }
    }

    /// Assert the tokeniser's output covers `0..text.len()` with
    /// no gaps, no overlaps, and no zero-width events. This is
    /// the load-bearing invariant m1c-3's `SCI_SETSTYLING` pass
    /// depends on for correct paint.
    fn assert_covers_buffer(text: &[u8], events: &[StyleEvent]) {
        if text.is_empty() {
            assert!(events.is_empty(), "empty buffer must emit no events");
            return;
        }
        assert_eq!(
            events.first().unwrap().start,
            0,
            "first event must start at 0"
        );
        assert_eq!(
            events.last().unwrap().end,
            text.len(),
            "last event must end at buffer length"
        );
        for pair in events.windows(2) {
            assert_eq!(
                pair[0].end, pair[1].start,
                "adjacent events must tile (no gap, no overlap)"
            );
        }
        for event in events {
            assert!(
                event.end > event.start,
                "zero-width event at {}",
                event.start
            );
        }
    }

    // --- Line comments ---------------------------------------

    #[test]
    fn line_comment_covers_hash_to_newline() {
        let def = synth_udl("00# 01 02((EOL))", "");
        let t = Tokeniser::new(&def);
        let text = b"foo\n# bar\nbaz";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        // The `#` marker at byte 4 triggers CommentLine covering
        // through the newline at byte 9.
        let comment = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::CommentLine)
            .expect("must find a line-comment event");
        assert_eq!(comment.start, 4);
        assert_eq!(comment.end, 10); // includes trailing \n
    }

    #[test]
    fn line_comment_unterminated_ok_at_eof() {
        // No trailing newline — the event still emits, ending
        // at the buffer end.
        let def = synth_udl("00# 02((EOL))", "");
        let t = Tokeniser::new(&def);
        let text = b"# unterminated";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].slot, UdlStyleSlot::CommentLine);
        assert_eq!(events[0].end, text.len());
    }

    // --- Block comments --------------------------------------

    #[test]
    fn block_comment_covers_open_to_close_inclusive() {
        let def = synth_udl("03<!-- 04-->", "");
        let t = Tokeniser::new(&def);
        let text = b"a<!-- hello -->b";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        let comment = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Comment)
            .expect("must find a block-comment event");
        assert_eq!(comment.start, 1);
        assert_eq!(comment.end, 15); // through the `-->` at bytes 12..=14
    }

    #[test]
    fn unterminated_block_comment_consumes_rest_of_buffer() {
        let def = synth_udl("03/* 04*/", "");
        let t = Tokeniser::new(&def);
        let text = b"a/* runaway";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        let comment = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Comment)
            .expect("must find an (unterminated) block-comment event");
        assert_eq!(comment.start, 1);
        assert_eq!(comment.end, text.len());
    }

    // --- Delimiters ------------------------------------------

    #[test]
    fn simple_double_quoted_string_paints_as_delimiter1() {
        // A single delimiter pair with `"` open and `"` close.
        let def = synth_udl("", "00\" 02\"");
        let t = Tokeniser::new(&def);
        let text = b"a \"hello\" b";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        let string = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Delimiter1)
            .expect("must find a Delimiter1 span");
        assert_eq!(string.start, 2);
        assert_eq!(string.end, 9); // through the closing `"`
    }

    #[test]
    fn delimiter_closes_at_eol_when_close_is_eol_marker() {
        // `((EOL))` as delimiter close — one-line-only span,
        // matching what markdown uses for `*italic*` etc.
        let def = synth_udl("", "00\" 02((EOL))");
        let t = Tokeniser::new(&def);
        let text = b"\"unterminated\nnew line";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        let string = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Delimiter1)
            .expect("must find a Delimiter1 span");
        assert_eq!(string.start, 0);
        assert_eq!(string.end, 14); // through the newline byte
    }

    #[test]
    fn multiple_delimiters_use_correct_slot_indices() {
        // Delimiter 0 with `"..."`, delimiter 1 with `'...'`.
        let def = synth_udl("", "00\" 02\" 03' 05'");
        let t = Tokeniser::new(&def);
        let text = b"\"a\" 'b'";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);

        let d1 = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Delimiter1)
            .expect("Delimiter1 for double-quoted string");
        assert_eq!(&text[d1.start..d1.end], b"\"a\"");

        let d2 = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Delimiter2)
            .expect("Delimiter2 for single-quoted string");
        assert_eq!(&text[d2.start..d2.end], b"'b'");
    }

    // --- Coalescing ------------------------------------------

    #[test]
    fn adjacent_default_events_coalesce() {
        // Plain text with no comment/delimiter should emit ONE
        // Default event covering the whole buffer, not N events.
        let def = synth_udl("00# 02((EOL))", "");
        let t = Tokeniser::new(&def);
        let text = b"plain text here";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].slot, UdlStyleSlot::Default);
    }

    // --- Integration -----------------------------------------

    #[test]
    fn markdown_udl_end_to_end_smoke() {
        // Load the real markdown UDL, tokenise a small sample,
        // confirm the code-fence delimiter opens and the # line
        // comment fires.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("preinstalled-udls")
            .join("markdown._preinstalled.udl.xml");
        let def = crate::UdlDefinition::from_file(&path).expect("markdown UDL parses");
        let t = Tokeniser::new(&def);
        let text = b"# heading\ntext ```\ncode\n```\n";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        // Markdown's line-comment marker is `#`, so the first
        // line paints as CommentLine.
        assert!(
            events.iter().any(|e| e.slot == UdlStyleSlot::CommentLine),
            "must find at least one CommentLine event"
        );
    }

    // --- Empty buffer ----------------------------------------

    #[test]
    fn empty_buffer_yields_no_events() {
        let def = synth_udl("00# 02((EOL))", "00\" 02\"");
        let t = Tokeniser::new(&def);
        let events = t.tokenise(b"");
        assert!(events.is_empty());
    }

    // --- Slot mapping ----------------------------------------

    #[test]
    fn delimiter_slot_indices_map_correctly() {
        for (idx, expected) in [
            (0, UdlStyleSlot::Delimiter1),
            (1, UdlStyleSlot::Delimiter2),
            (2, UdlStyleSlot::Delimiter3),
            (3, UdlStyleSlot::Delimiter4),
            (4, UdlStyleSlot::Delimiter5),
            (5, UdlStyleSlot::Delimiter6),
            (6, UdlStyleSlot::Delimiter7),
            (7, UdlStyleSlot::Delimiter8),
        ] {
            assert_eq!(UdlStyleSlot::from_delim_idx(idx), expected);
        }
    }

    #[test]
    fn longest_alternative_wins_regardless_of_udl_source_order() {
        // Regression pin for the alternative-ordering discipline
        // (module-level doc). UDL lists opener alternatives in
        // arbitrary order — the tokeniser must sort by descending
        // length so a `<` opener declared before `<!--` doesn't
        // silently truncate an HTML-comment match.
        let def = synth_udl("", "00< 00<!-- 02> 02-->");
        let t = Tokeniser::new(&def);
        let text = b"<!-- test -->";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        // The whole `<!-- test -->` span must be one delimiter
        // event — NOT a truncated `<` opener + `!-- test --` +
        // truncated `>` closer.
        let d = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Delimiter1)
            .expect("must find Delimiter1 span");
        assert_eq!(&text[d.start..d.end], b"<!-- test -->");
    }

    #[test]
    fn line_comment_wins_over_delimiter_when_marker_shared() {
        // Precedence pin: if the same literal is declared as both
        // a line-comment marker and a delimiter opener, the line
        // comment fires (matches the documented order in
        // `Tokeniser::tokenise`).
        let def = synth_udl("00# 02((EOL))", "00# 02#");
        let t = Tokeniser::new(&def);
        let text = b"# text";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        assert!(events.iter().any(|e| e.slot == UdlStyleSlot::CommentLine));
        assert!(!events.iter().any(|e| e.slot == UdlStyleSlot::Delimiter1));
    }

    #[test]
    fn multi_byte_utf8_content_survives_default_fill() {
        // The Default per-byte fill loop indexes `bytes: &[u8]`
        // (not `&str`), so multi-byte UTF-8 sequences pass
        // through unmodified. Pin the invariant: coverage still
        // holds byte-for-byte over emoji / CJK content.
        let def = synth_udl("00# 02((EOL))", "");
        let t = Tokeniser::new(&def);
        // "héllo 🌍 日本語" in UTF-8.
        let text = "héllo 🌍 日本語".as_bytes();
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        // No comment marker fires → one merged Default event.
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].slot, UdlStyleSlot::Default);
        assert_eq!(events[0].end, text.len());
    }

    #[test]
    fn markdown_fenced_code_currently_closes_at_first_newline() {
        // Documented known-simplification (module doc line 28-32
        // and the "Not covered yet" list). Pin the CURRENT
        // approximate behavior so a future EOL-except refinement
        // has a regression flag to trip when it changes.
        let def = synth_udl("", "00``` 02((EOL ```))");
        let t = Tokeniser::new(&def);
        let text = b"```\ncode\n```\n";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        // Today: delimiter closes at the first `\n` because we
        // treat `((EOL X))` as `((EOL))`. The whole span is just
        // ` ``` \n`. When the except-list refinement lands, this
        // test will fail — that's the point of the pin.
        let d = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Delimiter1)
            .expect("Delimiter1 span");
        assert_eq!(&text[d.start..d.end], b"```\n");
    }

    #[test]
    fn style_slot_discriminants_match_n_plus_plus_indices() {
        // Regression pin: the numeric discriminants must match
        // N++'s SCE_USER_STYLE_* order so m1c-3 can pass them
        // straight to SCI_SETSTYLING without a translation
        // table.
        assert_eq!(UdlStyleSlot::Default as u8, 0);
        assert_eq!(UdlStyleSlot::Comment as u8, 1);
        assert_eq!(UdlStyleSlot::CommentLine as u8, 2);
        assert_eq!(UdlStyleSlot::Number as u8, 3);
        assert_eq!(UdlStyleSlot::Word1 as u8, 4);
        assert_eq!(UdlStyleSlot::Word8 as u8, 11);
        assert_eq!(UdlStyleSlot::Operator as u8, 12);
        assert_eq!(UdlStyleSlot::Delimiter1 as u8, 16);
        assert_eq!(UdlStyleSlot::Delimiter8 as u8, 23);
    }
}
