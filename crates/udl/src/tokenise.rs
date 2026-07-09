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
//! **Covered**: line comments (with multiple opener markers —
//! Cisco IOS's `"00! 00remark"` fires for both), block
//! comments, delimiter pairs (including `((EOL))` and
//! `((EOL <chars>))` closers treated as end-of-line — the
//! except-list is honoured in a later refinement), keyword
//! classes (`Keywords1..=8` with `case_ignored` and per-class
//! `prefix` mode honoured), and Default fill for everything
//! else.
//!
//! **Not covered yet** (each is its own follow-up commit —
//! adding them on top of the scaffold below is additive, none
//! of the below has to change):
//! - Operators (`Operators1` / `Operators2`) — declared word
//!   boundaries in some UDLs (Cisco IOS uses `.` and `/`) but
//!   the current tokeniser treats them as ordinary default
//!   fill. Visible impact: operator characters render in the
//!   Default colour rather than the OPERATORS palette.
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
//!   delimiters. Keyword-class matching honours `case_ignored`;
//!   comment/delimiter openers are still byte-compared. Every
//!   real-world fixture uses ASCII-symbol markers (`#`, `//`,
//!   `!`, `[`, `"`) with no case dimension, so this doesn't
//!   currently mismatch any known UDL — but a hypothetical UDL
//!   with letter-based comment markers plus `case_ignored=yes`
//!   would fail.
//! - Word-boundary check on **delimiter** openers. Line-comment
//!   markers get one (see `match_line_comment`) because Cisco
//!   IOS's `remark` marker forced the issue: an identifier
//!   with `remark` as a leading substring would otherwise
//!   false-fire a comment. Delimiter openers have the same
//!   theoretical hazard but no known real UDL uses an
//!   alpha-prefixed delimiter, so the fix is deferred to keep
//!   this slice focused.
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

/// One keyword class (`Keywords1..=8`) pre-processed for
/// tokeniser lookup: sorted word list + prefix-mode flag +
/// destination style slot. Prepared once at [`Tokeniser::new`]
/// so per-token lookup is a binary search rather than a linear
/// scan of the raw whitespace-separated string.
///
/// When [`Tokeniser::case_ignored`] is true, [`Self::words`]
/// contains ASCII-lowercased copies of the source words and
/// the tokeniser lowercases each candidate identifier before
/// the search. Non-ASCII characters pass through unchanged —
/// N++'s own UDL editor writes keyword lists in the user's
/// locale, but every fixture I have inspected uses ASCII-only
/// keywords.
#[derive(Debug, Clone)]
struct KeywordClass {
    /// `Word1`..=`Word8` — the style slot to emit on a hit.
    slot: UdlStyleSlot,
    /// `true` iff the class was declared with `Keywords<N>="yes"`
    /// in `<Prefix>`. In prefix mode, an entry matches when it
    /// is a prefix of the current identifier (not just an exact
    /// match). Style is applied to the entire identifier (from
    /// its first byte up to the next non-word byte) regardless
    /// of the prefix length. This "whole-identifier styled"
    /// interpretation isn't established from N++'s public source
    /// — it's a judgement call about how e.g. Cisco IOS's
    /// `Keywords5` should paint `FastEthernet0/0`. Bounded
    /// failure mode: mis-assigned styling on the tail bytes
    /// past the prefix, never a crash or downstream mis-
    /// tokenisation.
    prefix_mode: bool,
    /// Words sorted ascending for binary-search lookup. Each
    /// entry is at most [`MAX_KEYWORD_BYTES`] bytes.
    words: Vec<String>,
}

/// Maximum byte length of a single keyword. Excess-length words
/// are dropped with a `tracing::warn!` at [`Tokeniser::new`].
///
/// **`DoS` defence.** In prefix mode, the tokeniser scans the
/// class's word list at every identifier start; without a per-
/// word length cap, a hostile UDL packing multi-KB "words"
/// would drive per-identifier cost up while the m1c-2 tokeniser
/// runs synchronously on the UI thread from `SCN_STYLENEEDED`.
///
/// **256** is generous relative to any real-world keyword —
/// the longest entry in the Cisco IOS UDL is ~30 chars
/// (`local-authentication`), Edditoria's markdown fixture tops
/// out at ~10 chars per entry.
const MAX_KEYWORD_BYTES: usize = 256;

/// Cap on the number of words per keyword class. Excess words
/// are dropped with a `tracing::warn!` at [`Tokeniser::new`].
///
/// **`DoS` defence.** In prefix mode, per-identifier lookup is
/// linear-time in the class size after binary-search narrowing
/// (still `O(1)` in the common case, `O(log N)` amortised).
/// Without a cap, a hostile UDL packing tens of thousands of
/// entries at one class could still drive worst-case tokenise
/// time up.
///
/// **8192** is a generous upper bound — the entire Cisco IOS
/// UDL declares ~600 words across all 6 populated classes;
/// even large language-server keyword dumps sit under 2000.
const MAX_KEYWORDS_PER_CLASS: usize = 8192;

/// Pre-compiled UDL rule tables — comment / delimiter parses
/// plus the eight pre-sorted [`KeywordClass`] tables — ready
/// for tokenise-loop consumption. Build exactly once per loaded
/// UDL (see [`crate::UdlEntry::compiled`]) and share across
/// every subsequent tokenise pass.
///
/// **Why this is a separate type.** Every `SCN_STYLENEEDED`
/// notification triggers a tokenise pass. If the compact-rule
/// parse (`CommentRules::parse` +
/// `DelimiterRules::parse` + `build_keyword_classes`, together
/// up to ~65 K allocations under maximally-packed keyword
/// caps) ran per-call, DESIGN.md §8's <5 ms p99 keystroke
/// budget would break trivially for a hostile UDL that just
/// stays under Phase 4.6 m1's 256 KiB file-size cap. Splitting
/// the parse into a load-time build (this type, called from
/// [`crate::registry::UdlRegistry::scan_dir`] and stored on
/// each entry) and a per-notification wrap ([`Tokeniser::new`]
/// = single pointer copy) keeps the hot path linear-in-text.
#[derive(Debug, Clone)]
pub struct UdlCompiledRules {
    comment: CommentRules,
    delims: DelimiterRules,
    /// Pre-processed `Keywords1..=8`. Index `k` is class `k+1`
    /// (Word slot `k+4`). Empty `words` = class not declared.
    keyword_classes: [KeywordClass; 8],
    /// Mirrors [`crate::UdlSettings::case_ignored`]. Cached on
    /// the compiled rules so the per-identifier hot path
    /// doesn't chase a pointer.
    case_ignored: bool,
}

impl UdlCompiledRules {
    /// Compile the raw UDL rules once. Sorts alternatives by
    /// descending literal length so longest-match wins (see the
    /// module-level "Alternative-ordering discipline"), and
    /// prepares each of the eight keyword classes via
    /// [`build_keyword_classes`].
    ///
    /// See the type-level docstring for why this is expected to
    /// run exactly once per loaded UDL rather than per-tokenise.
    #[must_use]
    pub fn new(def: &UdlDefinition) -> Self {
        let mut delims = DelimiterRules::parse(&def.keyword_lists.delimiters);
        for rule in &mut delims.rules {
            sort_by_descending_literal_len(&mut rule.open);
            sort_by_descending_literal_len(&mut rule.close);
        }
        let mut comment = CommentRules::parse(&def.keyword_lists.comments);
        // `line_open` accumulates alternatives (see docstring
        // on the field). Longest-match discipline matters when
        // a shorter marker is a prefix of a longer one.
        sort_by_descending_literal_len(&mut comment.line_open);
        let case_ignored = def.settings.case_ignored;
        let keyword_classes =
            build_keyword_classes(&def.keyword_lists.keywords, def.prefix, case_ignored);
        Self {
            comment,
            delims,
            keyword_classes,
            case_ignored,
        }
    }
}

/// Owning tokeniser — a shared borrow of already-built
/// [`UdlCompiledRules`]. Building a tokeniser is now a pointer
/// copy; the expensive parse happened once at
/// [`UdlCompiledRules::new`].
///
/// Not `Clone` because the borrowed reference guides the
/// lifetime discipline; the intended pattern is the
/// `SCN_STYLENEEDED` handler building a `Tokeniser` per
/// notification off the cached `Arc<UdlCompiledRules>` — cheap
/// and correct.
#[derive(Debug)]
pub struct Tokeniser<'a> {
    rules: &'a UdlCompiledRules,
}

impl<'a> Tokeniser<'a> {
    /// Wrap a pre-compiled rule set. See [`UdlCompiledRules`]
    /// for how the rules are built and shared.
    #[must_use]
    pub const fn new(rules: &'a UdlCompiledRules) -> Self {
        Self { rules }
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
            // outrank block, both outrank delimiters, delimiters
            // outrank keywords).
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
            if let Some((slot, len)) = self.match_keyword(text, i) {
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

    /// If any line-comment opener matches at `pos`, return the
    /// total span length (from opener start to line end). Else
    /// `None`. Multiple opener alternatives (Cisco IOS's
    /// `"00! 00remark"`) are tried in descending-length order
    /// (set up by [`Self::new`]), so a UDL with overlapping
    /// prefixes matches the longest first — no known real UDL
    /// exercises this but the discipline is symmetric with the
    /// delimiter-opener path.
    ///
    /// Line-comment span always terminates at the next `\n`
    /// (or end-of-buffer), regardless of the
    /// [`CommentRules::line_close`] variant —
    /// [`Sequence::EndOfLine`] is the only value markdown / C /
    /// SQL / every other mainstream UDL actually uses, and
    /// honouring a literal `line_close` sequence would need a
    /// lookahead scan the existing fixtures never exercise.
    fn match_line_comment(&self, text: &[u8], pos: usize) -> Option<usize> {
        for open_seq in &self.rules.comment.line_open {
            let Some(open) = literal_str(open_seq) else {
                continue;
            };
            let bytes = open.as_bytes();
            if !text[pos..].starts_with(bytes) {
                continue;
            }
            // Word-boundary discipline for alphabetic markers.
            // Cisco IOS declares `remark` alongside `!` as a
            // line-comment marker; without this guard, an
            // identifier containing `remark` as a leading
            // substring (`xremark yz`) would false-fire a
            // comment starting mid-identifier. Symbol markers
            // (`!`, `#`, `//`) are unaffected because
            // `is_word_byte(b'!')` etc. is false, so no guard
            // is applied to them.
            let first_byte_is_word = bytes.first().is_some_and(|b| is_word_byte(*b));
            if first_byte_is_word && pos > 0 && is_word_byte(text[pos - 1]) {
                continue;
            }
            // Walk to the next `\n` or end of buffer. The event
            // includes the newline byte if present so adjacent
            // events stay tight (the next line's event starts
            // at the byte after the newline).
            let mut i = pos + open.len();
            while i < text.len() && text[i] != b'\n' {
                i += 1;
            }
            if i < text.len() {
                i += 1; // include the newline byte
            }
            return Some(i - pos);
        }
        None
    }

    /// If a block-comment opener matches at `pos`, return the
    /// total span length up to and including the block-close
    /// literal (or to end-of-buffer if no closer is found).
    fn match_block_comment(&self, text: &[u8], pos: usize) -> Option<usize> {
        let open = literal_str(&self.rules.comment.block_open)?;
        let close = literal_str(&self.rules.comment.block_close)?;
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
        for (idx, rule) in self.rules.delims.rules.iter().enumerate() {
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

    /// If `pos` is at the start of an identifier that matches an
    /// entry in one of the eight keyword classes, return
    /// `Some((slot, identifier_len))`. Class 1 wins over class 2
    /// on a tie — first-match discipline, matching how the
    /// keyword lookup falls through the eight lists.
    ///
    /// **Identifier boundaries.** A byte is a word byte iff it
    /// is ASCII alphanumeric or `_`. `pos` must be at a word
    /// byte AND (`pos == 0` OR the previous byte is a non-word
    /// byte) — otherwise we're in the middle of an identifier
    /// the caller is walking through byte-by-byte, and matching
    /// mid-identifier would silently mis-highlight (e.g. treating
    /// `local` inside `localauth` as a keyword hit).
    ///
    /// **`case_ignored`.** When set, the candidate identifier is
    /// ASCII-lowercased before the lookup; the class word lists
    /// were pre-lowercased in [`build_keyword_classes`] to
    /// match. Non-ASCII bytes pass through unchanged in both
    /// directions — every real UDL uses ASCII-only keywords, so
    /// this simplification doesn't misbehave for any known
    /// fixture.
    ///
    /// **Prefix mode.** A class marked with `Keywords<N>="yes"`
    /// in `<Prefix>` matches when any of its entries is a prefix
    /// of the identifier; the whole identifier (up to the next
    /// non-word byte) then gets the class's style. Cisco IOS's
    /// `Keywords5` uses this to style
    /// `FastEthernet0/0`/`GigabitEthernet0/1` variants by
    /// matching just the shared `FastEthernet` / `GigabitEthernet`
    /// prefix.
    fn match_keyword(&self, text: &[u8], pos: usize) -> Option<(UdlStyleSlot, usize)> {
        // Word-boundary discipline: reject mid-identifier calls.
        if !is_word_byte(*text.get(pos)?) {
            return None;
        }
        if pos > 0 && is_word_byte(text[pos - 1]) {
            return None;
        }
        // Scan forward to identifier end (first non-word byte
        // or end-of-buffer).
        let mut end = pos + 1;
        while end < text.len() && is_word_byte(text[end]) {
            end += 1;
        }
        let ident = &text[pos..end];
        // Non-UTF-8 identifiers can't participate in keyword
        // lookup — every real UDL uses ASCII words. Guard so we
        // never index into `words` with garbage bytes.
        let ident_str = std::str::from_utf8(ident).ok()?;
        // Build a case-folded candidate once (rather than per
        // class) — most identifiers try every class.
        let folded_candidate;
        let candidate: &str = if self.rules.case_ignored {
            folded_candidate = ident_str.to_ascii_lowercase();
            &folded_candidate
        } else {
            ident_str
        };
        for class in &self.rules.keyword_classes {
            if class.words.is_empty() {
                continue;
            }
            if class.prefix_mode {
                if any_word_is_prefix_of(&class.words, candidate) {
                    return Some((class.slot, end - pos));
                }
            } else if class
                .words
                .binary_search_by(|w| w.as_str().cmp(candidate))
                .is_ok()
            {
                return Some((class.slot, end - pos));
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

/// Byte-level word predicate used by keyword-class matching.
/// ASCII alphanumeric or `_`. Simple, fast, and matches what
/// every N++ keyword-list in the wild expects — the fixture
/// UDLs (Cisco IOS, markdown, community collection) all
/// declare ASCII identifiers.
#[inline]
const fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `true` iff any entry in `sorted_words` is a prefix of
/// `candidate`.
///
/// **Algorithm.** For each byte-position `end` in
/// `1..=candidate.len()` that falls on a UTF-8 char boundary,
/// binary-search `sorted_words` for `candidate[..end]` as an
/// exact word. If found, that word is by construction a prefix
/// of `candidate`.
///
/// O(L × log N) where `L` is candidate byte-length and `N` is
/// the sorted-word list size — fast enough that the per-
/// identifier cost stays inside DESIGN.md §8's <5 ms
/// keystroke budget on a 8192-word class × 32-byte identifier
/// (~416 comparisons).
///
/// **Why the naïve "binary-search largest word ≤ candidate,
/// check prefix" doesn't work.** A word can be lexicographically
/// ≤ `candidate` without being a prefix. Counterexample from
/// the security-audit finding on the first attempt at this
/// function: `sorted_words = ["Ada", "Adal"]`, `candidate =
/// "Adam"`. `"Adal" ≤ "Adam"` (differ at the 4th byte,
/// `l < m`), so `partition_point` returns `2`, the "largest
/// word ≤ candidate" is `"Adal"`, and `"Adam".starts_with(
/// "Adal")` is false. The naïve algorithm returns false —
/// but `"Ada"` genuinely IS a prefix of `"Adam"`. The bug:
/// a non-prefix word can sort BETWEEN a true prefix and the
/// candidate.
///
/// The per-prefix-length algorithm above is correct by
/// construction — we ask the question "is the exact string
/// `candidate[..end]` in the word list?" rather than the
/// question "is any word a prefix of `candidate`?", turning a
/// range check into a set membership check where binary search
/// is unambiguously correct.
fn any_word_is_prefix_of(sorted_words: &[String], candidate: &str) -> bool {
    if sorted_words.is_empty() {
        return false;
    }
    for end in 1..=candidate.len() {
        // ASCII bytes are all char boundaries; the check is
        // load-bearing only for callers passing non-ASCII
        // identifiers, which `is_word_byte`'s ASCII-only
        // predicate prevents in practice.
        if !candidate.is_char_boundary(end) {
            continue;
        }
        let prefix = &candidate[..end];
        if sorted_words
            .binary_search_by(|w| w.as_str().cmp(prefix))
            .is_ok()
        {
            return true;
        }
    }
    false
}

/// Build eight [`KeywordClass`] entries from the raw whitespace-
/// separated keyword strings on [`crate::UdlKeywordLists`].
/// Called once from [`Tokeniser::new`]; results are cached on
/// the tokeniser for the buffer's lifetime.
///
/// Discipline:
/// - Empty word lists → `words: Vec::new()` (matcher skips).
/// - `case_ignored` → each word is ASCII-lowercased and the
///   matcher lowercases candidates before comparison.
/// - Words are deduplicated (keeping first occurrence) then
///   sorted ascending for [`any_word_is_prefix_of`]'s
///   binary-search precondition.
/// - Per-word length capped at [`MAX_KEYWORD_BYTES`]; per-class
///   count capped at [`MAX_KEYWORDS_PER_CLASS`]. Excess drops
///   log at `warn`.
fn build_keyword_classes(
    raw: &[String; 8],
    prefix_flags: [bool; 8],
    case_ignored: bool,
) -> [KeywordClass; 8] {
    let slots = [
        UdlStyleSlot::Word1,
        UdlStyleSlot::Word2,
        UdlStyleSlot::Word3,
        UdlStyleSlot::Word4,
        UdlStyleSlot::Word5,
        UdlStyleSlot::Word6,
        UdlStyleSlot::Word7,
        UdlStyleSlot::Word8,
    ];
    std::array::from_fn(|i| KeywordClass {
        slot: slots[i],
        prefix_mode: prefix_flags[i],
        words: normalise_class_words(&raw[i], case_ignored, i),
    })
}

/// Split the raw keyword string, cap lengths and counts,
/// lowercase (when `case_ignored`), dedupe, sort.
///
/// `class_idx` is used only for the `tracing::warn!` log
/// message so a user can identify which class dropped entries.
fn normalise_class_words(raw: &str, case_ignored: bool, class_idx: usize) -> Vec<String> {
    let mut words: Vec<String> = raw
        .split_whitespace()
        .filter_map(|w| {
            if w.len() > MAX_KEYWORD_BYTES {
                tracing::warn!(
                    class = class_idx + 1,
                    len = w.len(),
                    cap = MAX_KEYWORD_BYTES,
                    "UDL keyword exceeds byte cap; dropped \
                     (probable DoS-shaped input)"
                );
                return None;
            }
            Some(if case_ignored {
                w.to_ascii_lowercase()
            } else {
                w.to_owned()
            })
        })
        .collect();
    if words.len() > MAX_KEYWORDS_PER_CLASS {
        tracing::warn!(
            class = class_idx + 1,
            count = words.len(),
            cap = MAX_KEYWORDS_PER_CLASS,
            "UDL keyword class exceeds count cap; truncating \
             (probable DoS-shaped input)"
        );
        words.truncate(MAX_KEYWORDS_PER_CLASS);
    }
    words.sort();
    words.dedup();
    words
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

    /// Extension of [`synth_udl`] with per-class keyword lists,
    /// `case_ignored`, and per-class prefix-mode flags — used by
    /// the keyword-class tests. Every list not passed here
    /// defaults to empty.
    fn synth_udl_with_keywords(
        comments: &str,
        keywords: [&str; 8],
        prefix: [bool; 8],
        case_ignored: bool,
    ) -> UdlDefinition {
        let kw = crate::UdlKeywordLists {
            comments: comments.to_owned(),
            keywords: keywords.map(str::to_owned),
            ..crate::UdlKeywordLists::default()
        };
        UdlDefinition {
            name: "test".to_owned(),
            extensions: vec!["t".to_owned()],
            udl_version: "2.1".to_owned(),
            dark_mode_theme: false,
            settings: crate::UdlSettings {
                case_ignored,
                allow_fold_of_comments: false,
                fold_compact: false,
                force_pure_lc: 0,
                decimal_separator: 0,
            },
            prefix,
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
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

    // --- Multi-marker line comments --------------------------

    #[test]
    fn alphabetic_line_comment_marker_requires_word_boundary() {
        // Regression pin: Cisco IOS declares `remark` as one
        // of its line-comment markers. Without a word-boundary
        // check, `xremark yz` would false-fire a comment at
        // position 1, styling the rest of the line as
        // CommentLine even though `remark` is embedded in a
        // longer identifier. Confirm the guard blocks this.
        let def = synth_udl("00remark 02((EOL))", "");
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
        let text = b"xremark noise\n";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        assert!(
            !events.iter().any(|e| e.slot == UdlStyleSlot::CommentLine),
            "`remark` embedded inside an identifier must not open a comment"
        );
    }

    #[test]
    fn symbol_line_comment_marker_fires_regardless_of_neighbour() {
        // Companion pin for the word-boundary guard: symbol
        // markers (`!`, `#`, `//`) must still fire even when
        // the previous byte is a word byte — the word-boundary
        // check applies ONLY to markers whose first byte is
        // itself a word byte. Real-world example: mid-line
        // `!`-style comments in inline scripts.
        let def = synth_udl("00! 02((EOL))", "");
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
        let text = b"x!bang\n";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        // The `!` at position 1 opens a CommentLine span
        // through end-of-line.
        let hit = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::CommentLine)
            .expect("`!` after a word byte must still open a comment");
        assert_eq!(hit.start, 1);
        assert_eq!(hit.end, text.len());
    }

    #[test]
    fn multiple_line_comment_markers_both_fire() {
        // Regression pin for the Cisco IOS UDL — real v2.0 files
        // declare two line-comment markers via `00! 00remark`.
        // Both `!` and `remark` at line start must open a
        // CommentLine span. Line 1: `!` marker. Line 2:
        // `remark` marker. Line 3: unrelated text (default fill).
        //
        // Note: the two adjacent CommentLine spans coalesce into
        // one event via `push_event`'s trailing-event merge —
        // that's the intended tokeniser behaviour, so we test
        // per-position slot membership rather than per-event
        // boundaries.
        let def = synth_udl_with_keywords(
            "00! 00remark 02((EOL))",
            ["", "", "", "", "", "", "", ""],
            [false; 8],
            true,
        );
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
        let text = b"! bang line\nremark also comment\nplain line\n";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        let slot_at = |pos: usize| -> UdlStyleSlot {
            events
                .iter()
                .find(|e| pos >= e.start && pos < e.end)
                .expect("every position is covered")
                .slot
        };
        // Byte 0 = `!` — line 1 is CommentLine via `!` marker.
        assert_eq!(slot_at(0), UdlStyleSlot::CommentLine);
        // Byte 12 = `r` of "remark" — line 2 is CommentLine via
        // `remark` marker (would be Default under the pre-fix
        // last-wins behaviour, since `remark` overwriting `!`
        // wouldn't help — `!` was the surviving marker and
        // `remark` at line start wouldn't match it).
        assert_eq!(slot_at(12), UdlStyleSlot::CommentLine);
        // Byte 32 = `p` of "plain" — no marker matches → Default.
        assert_eq!(slot_at(32), UdlStyleSlot::Default);
    }

    // --- Keyword-class tokenisation --------------------------

    #[test]
    fn keyword_class_1_matches_and_styles_as_word1() {
        // Basic case-sensitive keyword. `if` in Keywords1 →
        // Word1 slot for that identifier only. The surrounding
        // `x` bytes stay Default.
        let def = synth_udl_with_keywords(
            "", // no comments
            ["if else return", "", "", "", "", "", "", ""],
            [false; 8],
            false, // case-sensitive
        );
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
        let text = b"x if x";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        let hit = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Word1)
            .expect("`if` must match Keywords1");
        assert_eq!(&text[hit.start..hit.end], b"if");
    }

    #[test]
    fn case_ignored_lowercases_before_comparison() {
        // With `caseIgnored="yes"` (via the `true` arg), the
        // keyword list is lowercased at Tokeniser::new-time and
        // candidate identifiers are lowercased before lookup.
        // Confirm `HOSTNAME` matches `hostname`.
        let def = synth_udl_with_keywords(
            "",
            ["hostname", "", "", "", "", "", "", ""],
            [false; 8],
            true,
        );
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
        let text = b"HOSTNAME device1";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        let hit = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Word1)
            .expect("`HOSTNAME` must match case-folded `hostname`");
        assert_eq!(&text[hit.start..hit.end], b"HOSTNAME");
    }

    #[test]
    fn prefix_mode_matches_prefix_of_identifier_and_styles_whole_token() {
        // Regression pin for Cisco IOS's `Keywords5` prefix mode:
        // `FastEthernet0/0` should style the identifier `FastEthernet0`
        // (up to the `/` boundary) as Word5 because `FastEthernet`
        // is a prefix. The trailing `/0` isn't part of the
        // identifier so it's not affected by the prefix rule.
        let def = synth_udl_with_keywords(
            "",
            [
                "",
                "",
                "",
                "",
                "FastEthernet GigabitEthernet Vlan", // Keywords5
                "",
                "",
                "",
            ],
            [false, false, false, false, true, false, false, false],
            true,
        );
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
        let text = b"interface FastEthernet0/0";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        let hit = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Word5)
            .expect("`FastEthernet0` must match Keywords5 prefix `FastEthernet`");
        assert_eq!(&text[hit.start..hit.end], b"FastEthernet0");
    }

    #[test]
    fn non_prefix_mode_requires_exact_word_match() {
        // Without prefix mode, `interfaced` must NOT match the
        // Keywords1 word `interface`. Prevents false positives
        // on words that happen to start with a keyword.
        let def = synth_udl_with_keywords(
            "",
            ["interface", "", "", "", "", "", "", ""],
            [false; 8],
            true,
        );
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
        let text = b"interfaced";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        assert!(
            !events.iter().any(|e| e.slot == UdlStyleSlot::Word1),
            "`interfaced` must not match `interface` without prefix mode"
        );
    }

    #[test]
    fn keyword_class_priority_class1_wins_over_class2() {
        // First-match discipline: if the same word appears in
        // both Keywords1 and Keywords2, class 1 wins. Matches
        // the class-order semantics N++ uses.
        let def = synth_udl_with_keywords(
            "",
            ["hostname", "hostname", "", "", "", "", "", ""],
            [false; 8],
            true,
        );
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
        let text = b"hostname router1";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        let hit = events
            .iter()
            .find(|e| e.slot == UdlStyleSlot::Word1 || e.slot == UdlStyleSlot::Word2)
            .expect("some class matches");
        assert_eq!(hit.slot, UdlStyleSlot::Word1);
        assert_eq!(&text[hit.start..hit.end], b"hostname");
    }

    #[test]
    fn keyword_match_requires_word_boundary_on_both_sides() {
        // A keyword sitting in the middle of a longer word
        // (`hostnamexyz`) must not match — otherwise a longer
        // identifier gets its head arbitrarily coloured. The
        // matcher already gates on word boundaries at both ends
        // via the pos>0/prev-byte check and the ident-end walk.
        let def =
            synth_udl_with_keywords("", ["host", "", "", "", "", "", "", ""], [false; 8], true);
        let rules = UdlCompiledRules::new(&def);
        let t = Tokeniser::new(&rules);
        // `xhosty` — no word boundary before or after `host`,
        // so no match anywhere in the buffer.
        let text = b"xhosty";
        let events = t.tokenise(text);
        assert_covers_buffer(text, &events);
        assert!(
            !events.iter().any(|e| e.slot == UdlStyleSlot::Word1),
            "no word-boundary means no keyword match"
        );
    }

    #[test]
    fn keyword_prefix_binary_search_matches_linear_scan() {
        // Property-style pin for `any_word_is_prefix_of`'s
        // correctness: for every candidate, the binary-search
        // result matches the naive linear "any-prefix" scan.
        //
        // Includes the security-audit counterexample from the
        // first attempt at this function (`["Ada", "Adal"]` vs
        // `"Adam"`) — the naïve "largest word ≤ candidate,
        // check prefix" algorithm returned false because
        // `"Adal"` (not a prefix of `"Adam"`) sorts between
        // the true prefix `"Ada"` and `"Adam"`. The correct
        // per-prefix-length algorithm above must return true.
        let sorted = vec![
            // Short prefix + longer sibling covers the
            // "Ada"/"Adam" pattern from the first audit round.
            "Ada".to_owned(),
            "Adal".to_owned(),
            // Second interloper family from the follow-up
            // review: `["F", "Fa", "Fc"]` vs candidate `"Fb"`.
            // `"Fa"` (not a prefix of `"Fb"`) is the largest
            // word ≤ `"Fb"`; a naïve "largest-word check
            // prefix" algorithm returns false — but `"F"` is a
            // genuine prefix. The corrected per-prefix-length
            // algorithm must return true.
            "F".to_owned(),
            "Fa".to_owned(),
            "Fc".to_owned(),
            "Fast".to_owned(),
            "FastEthernet".to_owned(),
            "Vlan".to_owned(),
        ];
        let candidates = [
            "",
            "A",
            "Ad",
            "Ada",
            "Adam", // <-- audit counterexample; must return true
            "Adal", // exact match on the interfering word
            "Adall",
            "Adama",
            "F",
            "Fa",
            "Fb", // <-- follow-up counterexample; must return true (`"F"` is a prefix)
            "Fc",
            "Fca",
            "Fast",
            "FastEthernet",
            "FastEthernet0",
            "FastEthernett",
            "V",
            "Vl",
            "Vlan",
            "VlanX",
            "Zzz",
        ];
        for c in candidates {
            let linear = sorted.iter().any(|w| c.starts_with(w.as_str()));
            let binary = any_word_is_prefix_of(&sorted, c);
            assert_eq!(binary, linear, "mismatch on candidate {c:?}");
        }
        // Empty list edge — never matches.
        let empty: Vec<String> = Vec::new();
        for c in candidates {
            assert!(!any_word_is_prefix_of(&empty, c));
        }
    }
}
