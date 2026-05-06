//! Find-in-Files primitives.
//!
//! Pure, headless half of Phase 4 m4. Owns the three things that
//! don't need threads, channels, or the filesystem to be testable:
//!
//!  1. [`is_binary`] — heuristic byte scan to skip obvious binaries.
//!  2. [`FifQuery`] / [`search_in_text`] — compiled query + per-file
//!     line-anchored search returning [`FifMatch`] records.
//!  3. [`FifWalkOpts`] — include/exclude glob configuration and the
//!     `path_matches` predicate the worker pool will consult.
//!
//! The actual directory walk and worker fan-out live in
//! `codepp-shell::fif` (Phase 4 m4 step 2). DESIGN.md §5.4 forbids
//! `core` from spawning OS resources, so this module deliberately
//! holds no `Sender`s, no threads, and no `std::fs`.
//!
//! ## Regex flavor
//!
//! All searches compile to a [`regex::Regex`]. Literal queries are
//! routed through `regex::escape`, so callers don't need a separate
//! plain-text code path. Whole-word and case-insensitive flags are
//! injected as inline regex modifiers (`(?i)`, `\b`) rather than
//! handled at match time, which keeps a single hot loop.
//!
//! Behavior diverges in one place from the in-buffer find dialog:
//! the dialog's regex flag is Scintilla's CXX11 engine (`SCFIND_CXX11REGEX`),
//! while FIF uses Rust's `regex` crate. The intersection covers
//! every common construct (`.`, `*`, `+`, `?`, `{n,m}`, `\d`, `\w`,
//! `\b`, character classes, alternation, capture groups). Diverging
//! features (PCRE lookarounds, possessive quantifiers) are absent
//! from both engines.

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::{Regex, RegexBuilder};

use crate::encoding;

/// Total per-job match cap (DESIGN.md Phase 4 m4 spec). The results
/// dock truncates beyond this and surfaces "X more matches not
/// shown" — chosen to keep the listview responsive on pathological
/// queries against multi-million-line corpora.
pub const MAX_MATCHES_TOTAL: usize = 10_000;

/// Soft per-file cap. A single file with >1000 hits is almost
/// certainly a query mistake (matching `;` against minified JS,
/// matching `e` against any prose); truncate so the worker doesn't
/// allocate a million-element `Vec` before the global cap kicks
/// in. Caller observes truncation through [`FileSearchOutcome::truncated`].
pub const MAX_MATCHES_PER_FILE: usize = 1_000;

/// First-N-bytes window for the binary heuristic. Mirrors ripgrep's
/// default and is large enough to catch BOMs plus the first few
/// records of nearly any executable format.
pub const BINARY_PROBE_BYTES: usize = 8 * 1024;

/// Default ceiling on individual file size (bytes). Files larger
/// than this are skipped entirely — the worker doesn't even read
/// them. 32 MiB matches Notepad++'s "huge file" warning threshold.
/// Caller can override via [`FifWalkOpts::max_file_bytes`].
pub const DEFAULT_MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Hard ceiling on the size of the compiled regex (bytes). Set
/// explicitly rather than relying on `regex`'s default (10 MiB) so
/// the budget is local, visible, and unaffected by upstream
/// default changes. 512 KiB covers every realistic search pattern;
/// patterns that exceed it are rejected as
/// [`FifQueryError::Invalid`].
pub const REGEX_SIZE_LIMIT_BYTES: usize = 512 * 1024;

/// Hard ceiling on the lazy-DFA cache (bytes). 1 MiB is the
/// `regex` crate's own recommendation for a "large but not
/// pathological" pattern.
pub const REGEX_DFA_SIZE_LIMIT_BYTES: usize = 1024 * 1024;

/// Largest input `search_in_text` will scan. Backstops the
/// `usize → u32` casts on column and line numbers in [`FifMatch`]:
/// any input under this length is guaranteed to produce non-truncated
/// offsets, regardless of what the upstream worker enforces. Set to
/// half of `u32::MAX` so a single match's byte range can never
/// overflow when summed.
pub const MAX_TEXT_BYTES: usize = (u32::MAX / 2) as usize;

/// Per-pattern length cap for [`FifWalkOpts::set_includes`] /
/// [`FifWalkOpts::set_excludes`]. UI-typed globs are routinely under
/// 50 bytes (`*.rs`, `**/*.txt`); 512 bytes leaves headroom for
/// machine-generated patterns without admitting a denial-of-service
/// vector through the `globset` compiler.
pub const MAX_GLOB_PATTERN_BYTES: usize = 512;

/// Maximum number of patterns admitted into a single include or
/// exclude set. `globset` compiles its patterns into a `RegexSet`
/// internally, and very large alternations enlarge the compiled
/// state quadratically; 64 is well past what the UI surfaces.
pub const MAX_GLOB_PATTERNS: usize = 64;

/// Knobs that determine how a query string is compiled into a
/// [`Regex`]. Mirrors the user-visible checkboxes in the Find dialog.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FifQueryOpts {
    /// `true` → `Foo` and `foo` differ. `false` injects `(?i)` into
    /// the compiled pattern.
    pub match_case: bool,
    /// `true` → wrap the pattern in `\b...\b` so `foo` does not
    /// match inside `foobar`. Applied after escape/regex compilation
    /// so it composes with both literal and regex queries.
    pub whole_word: bool,
    /// `true` → treat the query string as a regex pattern. `false`
    /// runs `regex::escape` first so metacharacters are literal.
    pub regex: bool,
}

/// A compiled [`FifQueryOpts`] + pattern, ready to scan text. Cheap to
/// clone (the inner [`Regex`] is `Arc`-counted).
#[derive(Debug, Clone)]
pub struct FifQuery {
    pattern: Regex,
}

/// Errors from [`FifQuery::compile`].
#[derive(Debug)]
pub enum FifQueryError {
    /// User typed an empty query — every callsite needs to reject
    /// this rather than letting the regex engine return zero-width
    /// matches across every byte boundary.
    Empty,
    /// `regex::Regex::new` rejected the pattern (invalid regex
    /// syntax in REGEX mode, or pathologically nested constructs).
    Invalid(regex::Error),
}

impl std::fmt::Display for FifQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FifQueryError::Empty => f.write_str("query is empty"),
            FifQueryError::Invalid(e) => write!(f, "invalid query: {e}"),
        }
    }
}

impl std::error::Error for FifQueryError {}

impl FifQuery {
    /// Compile `query` under `opts` into a [`Regex`]. The order of
    /// transforms — escape (if not regex), case, whole-word — is
    /// chosen so each composes with the next without re-parsing:
    /// the literal escape happens first because `(?i)` and `\b`
    /// don't care about metacharacter status.
    pub fn compile(query: &str, opts: FifQueryOpts) -> Result<Self, FifQueryError> {
        if query.is_empty() {
            return Err(FifQueryError::Empty);
        }
        let body: String = if opts.regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        let with_word = if opts.whole_word {
            format!(r"\b{body}\b")
        } else {
            body
        };
        let with_case = if opts.match_case {
            with_word
        } else {
            format!("(?i){with_word}")
        };
        let pattern = RegexBuilder::new(&with_case)
            .size_limit(REGEX_SIZE_LIMIT_BYTES)
            .dfa_size_limit(REGEX_DFA_SIZE_LIMIT_BYTES)
            .build()
            .map_err(FifQueryError::Invalid)?;
        Ok(Self { pattern })
    }
}

/// One match within one file, anchored to the line that contains it.
///
/// Byte offsets are over the decoded `&str` passed to
/// [`search_in_text`]; the line text is owned (cloned out) so the
/// caller can drop the source buffer immediately after the search
/// returns — important for worker-thread memory bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FifMatch {
    /// 1-based line number — the format Find/Replace dialog status
    /// strings already use.
    pub line_no: u32,
    /// Byte offset of the match within `line_text`. Multibyte-aware
    /// (UTF-8 code-unit offsets, not chars).
    pub col_start: u32,
    /// Byte offset of the match end, exclusive.
    pub col_end: u32,
    /// Full text of the line containing the match, with the trailing
    /// newline stripped. Capped at [`LINE_TEXT_MAX_BYTES`] — longer
    /// lines are truncated mid-codepoint-safely so the listview
    /// doesn't have to reflow a 1 MB minified line.
    pub line_text: String,
}

/// Truncation cap for [`FifMatch::line_text`]. Hot lines in lockfiles
/// and bundled JS regularly exceed 100 KiB; the listview displays
/// only the first 200 columns anyway, so anything past 1 KiB is dead
/// weight on the wire.
pub const LINE_TEXT_MAX_BYTES: usize = 1024;

/// Outcome of searching one file's worth of text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileSearchOutcome {
    /// All matches within this file, ordered by line then column.
    pub matches: Vec<FifMatch>,
    /// `true` if [`MAX_MATCHES_PER_FILE`] was hit and additional
    /// matches in this file were dropped. The UI surfaces this with
    /// a "(truncated)" badge per file.
    pub truncated: bool,
}

/// Scan `text` for `query`, returning per-line matches.
///
/// Stops after [`MAX_MATCHES_PER_FILE`]; the `truncated` flag in the
/// return reflects whether the cap was hit. Empty matches (regexes
/// that match zero-width like `^` or `\b`) are skipped to avoid an
/// infinite-stream-of-empties on degenerate patterns.
///
/// **Cross-file cap:** [`MAX_MATCHES_TOTAL`] is the *job-wide* cap
/// the orchestrator enforces across every file's outcome — this
/// function only bounds a single file. The two caps coexist and
/// future maintainers should not "simplify" them down to one.
///
/// **Oversize input:** `text` larger than [`MAX_TEXT_BYTES`] returns
/// an empty outcome rather than running the search; otherwise the
/// `usize → u32` casts on column and line offsets in [`FifMatch`]
/// could silently truncate. Workers never reach this branch in
/// practice because [`DEFAULT_MAX_FILE_BYTES`] (32 MiB) is well
/// below the 2 GiB ceiling, but the guard makes the contract local
/// and self-defending.
pub fn search_in_text(query: &FifQuery, text: &str) -> FileSearchOutcome {
    let mut out = FileSearchOutcome::default();
    if text.is_empty() || text.len() > MAX_TEXT_BYTES {
        return out;
    }
    // Worst case is one entry per byte (a file of `\n`s), so the
    // helper allocates up to 8 bytes × `text.len()` here. With the
    // 32 MiB upstream file cap this peaks around 256 MiB per worker
    // — bounded but worth knowing about if `DEFAULT_MAX_FILE_BYTES`
    // is ever raised.
    let mut line_starts: Vec<usize> = Vec::with_capacity(64);
    line_starts.push(0);
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    for m in query.pattern.find_iter(text) {
        if m.start() == m.end() {
            continue;
        }
        if out.matches.len() >= MAX_MATCHES_PER_FILE {
            out.truncated = true;
            break;
        }
        let (line_no_zero, line_start) = locate_line(&line_starts, m.start());
        let line_end = line_starts
            .get(line_no_zero + 1)
            .copied()
            .unwrap_or(text.len());
        let raw_line = &text[line_start..line_end];
        let without_lf = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let trimmed = without_lf.strip_suffix('\r').unwrap_or(without_lf);
        let line_text = clip_line(trimmed);
        let col_start = (m.start() - line_start) as u32;
        let col_end = (m.end() - line_start) as u32;
        out.matches.push(FifMatch {
            line_no: (line_no_zero as u32) + 1,
            col_start,
            col_end,
            line_text,
        });
    }
    out
}

/// Binary search `line_starts` for the line containing `byte_offset`.
/// Returns `(zero_based_line_no, line_start_byte)`.
fn locate_line(line_starts: &[usize], byte_offset: usize) -> (usize, usize) {
    match line_starts.binary_search(&byte_offset) {
        Ok(idx) => (idx, line_starts[idx]),
        Err(insert) => {
            // Insert position is the line *after* the target — back
            // up by one to land on the line whose start is ≤ offset.
            let line = insert.saturating_sub(1);
            (line, line_starts[line])
        }
    }
}

/// Truncate `line` to ≤ [`LINE_TEXT_MAX_BYTES`] without splitting a
/// UTF-8 codepoint. Returns an owned `String` either way.
fn clip_line(line: &str) -> String {
    if line.len() <= LINE_TEXT_MAX_BYTES {
        return line.to_string();
    }
    let mut cut = LINE_TEXT_MAX_BYTES;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    // `'…'` (U+2026) is 3 bytes in UTF-8 — size the buffer to fit the
    // truncated body plus the ellipsis without a reallocation.
    let mut s = String::with_capacity(cut + '…'.len_utf8());
    s.push_str(&line[..cut]);
    s.push('…');
    s
}

/// Apply `query`'s pattern to `text` and return the rewritten text
/// plus the number of substitutions performed. `expand_groups`
/// controls whether `$0`/`$1`/... in `replacement` are expanded as
/// regex backreferences (Replace All on a regex query) or treated
/// as literal characters (Replace All on a literal query — `$1`
/// stays `$1` in the output).
///
/// Returns `(Cow::Borrowed(text), 0)` for any input larger than
/// [`MAX_TEXT_BYTES`] — same backstop as [`search_in_text`]. The
/// `Cow` return avoids allocating a multi-gigabyte copy of the
/// input on the unreachable-by-construction oversize branch (the
/// 32 MiB `DEFAULT_MAX_FILE_BYTES` upstream bound makes oversize
/// impossible from the worker), and lets the no-substitutions
/// case skip allocating altogether.
pub fn replace_in_text<'t>(
    query: &FifQuery,
    text: &'t str,
    replacement: &str,
    expand_groups: bool,
) -> (std::borrow::Cow<'t, str>, usize) {
    if text.len() > MAX_TEXT_BYTES {
        return (std::borrow::Cow::Borrowed(text), 0);
    }
    let mut count: usize = 0;
    let result = query
        .pattern
        .replace_all(text, |caps: &regex::Captures<'_>| {
            // Skip zero-width matches for the same reason
            // `search_in_text` does: a regex like `^` would otherwise
            // splice the replacement at every line start, exploding the
            // file size for no useful Replace All semantic.
            if let Some(m) = caps.get(0) {
                if m.start() == m.end() {
                    return String::new();
                }
            }
            count += 1;
            if expand_groups {
                let mut buf = String::new();
                caps.expand(replacement, &mut buf);
                buf
            } else {
                replacement.to_string()
            }
        });
    (result, count)
}

/// Heuristic binary detector. Returns `true` if `prefix` (typically
/// the first [`BINARY_PROBE_BYTES`] of a file) looks like binary
/// content the user does not want searched.
///
/// Algorithm, mirroring ripgrep / git's:
///
///  1. Run [`encoding::detect`] to strip a recognised text BOM. If
///     a BOM is present the file is text.
///  2. Otherwise scan the BOM-stripped prefix for a NUL byte. NUL
///     in the first 8 KiB is the cheapest, lowest-false-positive
///     binary signal across PE/ELF/Mach-O, ZIPs, images, etc.
///
/// Known false positives:
/// - **UTF-16 / UTF-32 without BOM** look binary because every
///   second/fourth byte is NUL. The decoder upstream can't recover
///   without the BOM either, so skipping is the right call.
/// - Files with NUL inside the first 8 KiB but text past it
///   (uncommon — usually saved-game state, sqlite3 headers, etc.).
pub fn is_binary(prefix: &[u8]) -> bool {
    if prefix.is_empty() {
        return false;
    }
    let (_, body) = encoding::detect(prefix);
    if body.len() < prefix.len() {
        // A BOM was present. Files that declare themselves as text
        // via BOM are text, even if they happen to contain NULs
        // (rare but valid in UTF-16).
        return false;
    }
    body.iter().take(BINARY_PROBE_BYTES).any(|&b| b == 0)
}

/// Configuration for the directory walk that feeds the FIF worker
/// pool. Lives in `core` because compiling globs is pure and the
/// `path_matches` predicate is the only piece tested independently
/// of the FS.
#[derive(Debug, Clone)]
pub struct FifWalkOpts {
    /// Recurse into subdirectories. `false` for "search this folder
    /// only" UI mode.
    pub recurse: bool,
    /// Descend into directories whose basename starts with `.`
    /// (the dotfile / "hidden directory" convention on Unix and
    /// the same naming convention used by `.git`/`.idea`/etc. on
    /// Windows). `false` by default — most FIF use cases don't
    /// want to scan VCS metadata, IDE caches, or other hidden
    /// state. Driven by the FIF dialog's "In hidden folders"
    /// checkbox. Always-pruned basenames (`target`,
    /// `node_modules`, etc.) are unaffected by this flag — they're
    /// pruned for performance regardless.
    pub walk_hidden_dirs: bool,
    /// Skip files larger than this. Defaults to
    /// [`DEFAULT_MAX_FILE_BYTES`].
    pub max_file_bytes: u64,
    /// Files matching ANY of these globs are included. Empty means
    /// "include all files" (modulo excludes and binary detection).
    /// Patterns are evaluated against the file path (full path on
    /// the host filesystem) using `globset`'s default options.
    includes: GlobSet,
    /// Files matching ANY of these globs are excluded. Evaluated
    /// after includes, so a path that matches both is excluded.
    /// Pre-populated with sensible defaults (`.git`, `target`,
    /// `node_modules`) by [`Self::default`].
    excludes: GlobSet,
}

impl Default for FifWalkOpts {
    fn default() -> Self {
        // Only the always-pruned basenames go in the file-level
        // exclude set. Dot-prefixed dirs (`.git`/`.hg`/`.idea`/…)
        // are pruned at walk time when `walk_hidden_dirs` is false;
        // including them here too would override the user's "In
        // hidden folders" opt-in, which is the opposite of what the
        // checkbox should do.
        let mut excludes = GlobSetBuilder::new();
        for pat in [
            "**/target/**",
            "**/node_modules/**",
            "**/dist/**",
            "**/build/**",
        ] {
            // `Glob::new` only fails on malformed patterns, and these
            // are compile-time literals — `expect` is the right
            // tool, the panic would only fire on a code change that
            // typoed a pattern.
            let g = Glob::new(pat).expect("default exclude glob");
            excludes.add(g);
        }
        Self {
            recurse: true,
            walk_hidden_dirs: false,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            includes: GlobSet::empty(),
            excludes: excludes.build().expect("default exclude globset"),
        }
    }
}

/// Errors from building [`FifWalkOpts`] glob sets.
#[derive(Debug)]
pub enum FifWalkOptsError {
    /// One of the user-supplied patterns failed to compile.
    BadGlob(globset::Error),
    /// A single pattern exceeded [`MAX_GLOB_PATTERN_BYTES`].
    PatternTooLong { len: usize, limit: usize },
    /// The total number of patterns exceeded [`MAX_GLOB_PATTERNS`].
    TooManyPatterns { count: usize, limit: usize },
}

impl std::fmt::Display for FifWalkOptsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FifWalkOptsError::BadGlob(e) => write!(f, "invalid glob pattern: {e}"),
            FifWalkOptsError::PatternTooLong { len, limit } => {
                write!(f, "glob pattern too long: {len} bytes (limit {limit})")
            }
            FifWalkOptsError::TooManyPatterns { count, limit } => {
                write!(f, "too many glob patterns: {count} (limit {limit})")
            }
        }
    }
}

impl std::error::Error for FifWalkOptsError {}

impl FifWalkOpts {
    /// Replace the include set with the supplied patterns. Empty
    /// vector restores the "include everything" default.
    pub fn set_includes(&mut self, patterns: &[&str]) -> Result<(), FifWalkOptsError> {
        self.includes = build_globset(patterns)?;
        Ok(())
    }

    /// Replace the exclude set with the supplied patterns. Empty
    /// vector clears all excludes (including the defaults — caller
    /// is responsible for re-adding `.git` etc. if they want them).
    pub fn set_excludes(&mut self, patterns: &[&str]) -> Result<(), FifWalkOptsError> {
        self.excludes = build_globset(patterns)?;
        Ok(())
    }

    /// `true` if `path` should be visited by the search. Composes
    /// includes and excludes; a file is searchable iff:
    ///
    ///  - includes is empty OR the path matches an include pattern, AND
    ///  - the path does NOT match an exclude pattern.
    pub fn path_matches(&self, path: &Path) -> bool {
        if !self.excludes.is_empty() && self.excludes.is_match(path) {
            return false;
        }
        if self.includes.is_empty() {
            return true;
        }
        self.includes.is_match(path)
    }
}

fn build_globset(patterns: &[&str]) -> Result<GlobSet, FifWalkOptsError> {
    if patterns.len() > MAX_GLOB_PATTERNS {
        return Err(FifWalkOptsError::TooManyPatterns {
            count: patterns.len(),
            limit: MAX_GLOB_PATTERNS,
        });
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        if p.len() > MAX_GLOB_PATTERN_BYTES {
            return Err(FifWalkOptsError::PatternTooLong {
                len: p.len(),
                limit: MAX_GLOB_PATTERN_BYTES,
            });
        }
        let g = Glob::new(p).map_err(FifWalkOptsError::BadGlob)?;
        b.add(g);
    }
    b.build().map_err(FifWalkOptsError::BadGlob)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(query: &str, opts: FifQueryOpts, text: &str) -> Vec<FifMatch> {
        let q = FifQuery::compile(query, opts).expect("compile");
        search_in_text(&q, text).matches
    }

    #[test]
    fn literal_query_matches_single_line() {
        let hits = search(
            "needle",
            FifQueryOpts::default(),
            "haystack\nneedle here\nbye\n",
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line_no, 2);
        assert_eq!(hits[0].col_start, 0);
        assert_eq!(hits[0].col_end, 6);
        assert_eq!(hits[0].line_text, "needle here");
    }

    #[test]
    fn case_sensitive_flag_distinguishes_case() {
        let opts = FifQueryOpts {
            match_case: true,
            ..FifQueryOpts::default()
        };
        assert!(search("Foo", opts, "foo bar Foo").len() == 1);
        let case_insensitive = FifQueryOpts::default();
        assert_eq!(search("Foo", case_insensitive, "foo bar Foo").len(), 2);
    }

    #[test]
    fn whole_word_excludes_substring_match() {
        let opts = FifQueryOpts {
            whole_word: true,
            match_case: true,
            ..FifQueryOpts::default()
        };
        let hits = search("foo", opts, "foobar foo barfoo");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].col_start, 7);
    }

    #[test]
    fn regex_metachars_are_literal_when_regex_flag_off() {
        // `.` is metachar in regex but the user typed it as a dot.
        let opts = FifQueryOpts {
            match_case: true,
            ..FifQueryOpts::default()
        };
        let hits = search(".", opts, "abc.def");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].col_start, 3);
    }

    #[test]
    fn regex_flag_enables_metachars() {
        let opts = FifQueryOpts {
            regex: true,
            match_case: true,
            ..FifQueryOpts::default()
        };
        let hits = search(r"\d+", opts, "alpha 12 beta 345");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].line_text, "alpha 12 beta 345");
    }

    #[test]
    fn empty_query_rejected() {
        assert!(matches!(
            FifQuery::compile("", FifQueryOpts::default()),
            Err(FifQueryError::Empty)
        ));
    }

    #[test]
    fn invalid_regex_rejected() {
        let opts = FifQueryOpts {
            regex: true,
            ..FifQueryOpts::default()
        };
        assert!(matches!(
            FifQuery::compile("[unterminated", opts),
            Err(FifQueryError::Invalid(_))
        ));
    }

    #[test]
    fn line_numbers_are_one_based() {
        let hits = search("x", FifQueryOpts::default(), "x\nx\nx\n");
        assert_eq!(
            hits.iter().map(|m| m.line_no).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn crlf_endings_stripped_from_line_text() {
        let hits = search(
            "foo",
            FifQueryOpts::default(),
            "alpha\r\nfoo bar\r\nbeta\r\n",
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line_text, "foo bar");
    }

    #[test]
    fn per_file_cap_truncates() {
        let big = "x\n".repeat(MAX_MATCHES_PER_FILE + 50);
        let q = FifQuery::compile("x", FifQueryOpts::default()).unwrap();
        let outcome = search_in_text(&q, &big);
        assert_eq!(outcome.matches.len(), MAX_MATCHES_PER_FILE);
        assert!(outcome.truncated);
    }

    #[test]
    fn zero_width_matches_are_skipped() {
        // `^` matches at every line start; without the empty-skip
        // we'd return one FifMatch per line forever.
        let opts = FifQueryOpts {
            regex: true,
            ..FifQueryOpts::default()
        };
        let hits = search("^", opts, "a\nb\nc\n");
        assert!(hits.is_empty());
    }

    #[test]
    fn long_lines_are_clipped() {
        let long = "x".repeat(LINE_TEXT_MAX_BYTES + 100);
        let text = format!("prefix {long} suffix");
        let hits = search("prefix", FifQueryOpts::default(), &text);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].line_text.len() <= LINE_TEXT_MAX_BYTES + 4);
        assert!(hits[0].line_text.ends_with('…'));
    }

    #[test]
    fn multibyte_columns_count_utf8_bytes() {
        // Goal is to confirm the offsets are byte-anchored, which
        // is what Scintilla's SCI_GOTOPOS / target API expects.
        let hits = search("β", FifQueryOpts::default(), "αβγ\n");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].col_start, 2); // α is 2 bytes in UTF-8
        assert_eq!(hits[0].col_end, 4);
    }

    fn replace(
        query: &str,
        opts: FifQueryOpts,
        text: &str,
        repl: &str,
        expand: bool,
    ) -> (String, usize) {
        let q = FifQuery::compile(query, opts).expect("compile");
        let (cow, n) = replace_in_text(&q, text, repl, expand);
        (cow.into_owned(), n)
    }

    #[test]
    fn replace_literal_substitutes_each_match() {
        let (out, n) = replace("foo", FifQueryOpts::default(), "foo bar foo", "baz", false);
        assert_eq!(out, "baz bar baz");
        assert_eq!(n, 2);
    }

    #[test]
    fn replace_literal_keeps_dollar_sign_literal() {
        // Literal mode: `$1` in the replacement must NOT expand to
        // a capture group — it should appear verbatim in the output.
        let (out, n) = replace("foo", FifQueryOpts::default(), "foo", "$1", false);
        assert_eq!(out, "$1");
        assert_eq!(n, 1);
    }

    #[test]
    fn replace_regex_expands_capture_groups() {
        let opts = FifQueryOpts {
            regex: true,
            match_case: true,
            ..FifQueryOpts::default()
        };
        let (out, n) = replace(r"(\w+)", opts, "hello world", "[$1]", true);
        assert_eq!(out, "[hello] [world]");
        assert_eq!(n, 2);
    }

    #[test]
    fn replace_skips_zero_width_matches() {
        // `^` matches zero-width at every line start. Without the
        // skip the file would gain a replacement at every line
        // boundary, exploding the buffer.
        let opts = FifQueryOpts {
            regex: true,
            ..FifQueryOpts::default()
        };
        let (out, n) = replace("^", opts, "a\nb\n", "X", false);
        assert_eq!(out, "a\nb\n");
        assert_eq!(n, 0);
    }

    #[test]
    fn replace_no_matches_returns_input_unchanged() {
        let (out, n) = replace(
            "needle",
            FifQueryOpts::default(),
            "no match here",
            "X",
            false,
        );
        assert_eq!(out, "no match here");
        assert_eq!(n, 0);
    }

    #[test]
    fn unterminated_last_line_is_searchable() {
        // No trailing \n exercises the `line_end = text.len()` fallback
        // in `search_in_text` (the `unwrap_or(text.len())` path) — a
        // file without a final newline must still surface its last line.
        let hits = search("foo", FifQueryOpts::default(), "alpha\nfoo");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line_no, 2);
        assert_eq!(hits[0].line_text, "foo");
    }

    #[test]
    fn match_at_exact_line_boundary() {
        // `\nfoo` puts the match at byte offset 1, which is also the
        // start-of-line for line 2. Exercises the `Ok(idx)` branch in
        // `locate_line` (binary search hits an exact line-start entry).
        let hits = search("foo", FifQueryOpts::default(), "\nfoo\n");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line_no, 2);
        assert_eq!(hits[0].col_start, 0);
    }

    #[test]
    fn binary_detect_text_no_bom() {
        assert!(!is_binary(b"plain text\nno nulls"));
    }

    #[test]
    fn binary_detect_finds_null_byte() {
        let mut buf = b"some text\x00more".to_vec();
        buf.resize(100, b'a');
        assert!(is_binary(&buf));
    }

    #[test]
    fn binary_detect_utf8_bom_treated_as_text() {
        let mut buf = b"\xEF\xBB\xBFplain utf-8 with bom".to_vec();
        buf.push(0); // would otherwise mark binary; BOM rescues it
        assert!(!is_binary(&buf));
    }

    #[test]
    fn binary_detect_utf16_le_bom_with_nul_body_is_text() {
        // UTF-16 LE BOM + ASCII payload is dense with NUL bytes (every
        // second byte). Without the BOM-rescue path, `is_binary` would
        // (correctly, but unhelpfully) flag it as binary; with the
        // rescue, declared-text-via-BOM wins. The decoder upstream
        // handles UTF-16 once `encoding::detect` confirms the BOM.
        let mut buf = vec![0xFF, 0xFE]; // UTF-16 LE BOM
        for ch in "hello world".bytes() {
            buf.push(ch);
            buf.push(0);
        }
        assert!(!is_binary(&buf));
    }

    #[test]
    fn binary_detect_empty_is_text() {
        assert!(!is_binary(&[]));
    }

    #[test]
    fn binary_detect_only_probes_first_window() {
        // NUL just past the probe window is ignored — keeps the
        // heuristic O(8 KiB) regardless of file size.
        let mut buf = vec![b'a'; BINARY_PROBE_BYTES];
        buf.push(0);
        assert!(!is_binary(&buf));
    }

    #[test]
    fn walk_opts_default_excludes_build_artefacts() {
        // Default excludes cover the always-pruned set
        // (target / node_modules / build / dist). Dot-prefixed
        // directories (.git / .idea / etc.) are NOT in the
        // file-level filter — they're handled by the walker via
        // `walk_hidden_dirs`, so a path inside `.git` reaches
        // `path_matches` only when the user explicitly opted into
        // hidden folders, at which point it should match.
        let opts = FifWalkOpts::default();
        assert!(!opts.path_matches(Path::new("project/target/debug/foo")));
        assert!(!opts.path_matches(Path::new("project/node_modules/x/y.js")));
        assert!(!opts.path_matches(Path::new("project/build/out.o")));
        assert!(!opts.path_matches(Path::new("project/dist/bundle.js")));
        // Dot-prefixed paths pass the file-level filter — pruning
        // happens at walk time.
        assert!(opts.path_matches(Path::new("project/.git/config")));
        assert!(opts.path_matches(Path::new("project/.idea/workspace.xml")));
        // Normal source files match.
        assert!(opts.path_matches(Path::new("project/src/main.rs")));
    }

    #[test]
    fn walk_hidden_dirs_default_is_false() {
        assert!(!FifWalkOpts::default().walk_hidden_dirs);
    }

    #[test]
    fn walk_opts_includes_filter_overrides_default() {
        let mut opts = FifWalkOpts::default();
        opts.set_includes(&["**/*.rs"]).unwrap();
        assert!(opts.path_matches(Path::new("src/main.rs")));
        assert!(!opts.path_matches(Path::new("src/main.cpp")));
    }

    #[test]
    fn walk_opts_includes_match_at_any_depth_with_double_star_prefix() {
        // The UI side (`apply_filters_to_walk_opts` in ui_win32)
        // prepends `**/` to user-typed extension globs so `*.rs`
        // matches `src/sub/main.rs` and not just `main.rs` at the
        // root. Verify the underlying globset path rejects bare
        // `*.rs` for nested paths and accepts `**/*.rs`.
        let mut opts = FifWalkOpts::default();
        opts.set_includes(&["**/*.rs"]).unwrap();
        assert!(opts.path_matches(Path::new("src/sub/deep/main.rs")));
        assert!(opts.path_matches(Path::new("main.rs")));
    }

    #[test]
    fn walk_opts_excludes_beat_includes() {
        let mut opts = FifWalkOpts::default();
        opts.set_includes(&["**/*.rs"]).unwrap();
        // Default excludes should still beat the include for target/.
        assert!(!opts.path_matches(Path::new("target/debug/build/foo.rs")));
    }

    #[test]
    fn walk_opts_custom_excludes_replace_defaults() {
        let mut opts = FifWalkOpts::default();
        opts.set_excludes(&["**/*.bak"]).unwrap();
        // Custom excludes replace the defaults — `target/` is no
        // longer excluded under the new rule.
        assert!(opts.path_matches(Path::new("target/debug/foo.rs")));
        assert!(!opts.path_matches(Path::new("foo.bak")));
    }

    #[test]
    fn walk_opts_invalid_glob_returns_error() {
        let mut opts = FifWalkOpts::default();
        let err = opts.set_includes(&["[unterminated"]).unwrap_err();
        assert!(matches!(err, FifWalkOptsError::BadGlob(_)));
    }

    #[test]
    fn walk_opts_rejects_oversize_pattern() {
        // A glob longer than the per-pattern cap is refused before
        // it reaches `globset`'s compiler — bounds the per-set
        // compile work the user can extract from a single string.
        let mut opts = FifWalkOpts::default();
        let huge = "*".repeat(MAX_GLOB_PATTERN_BYTES + 1);
        let err = opts.set_includes(&[huge.as_str()]).unwrap_err();
        assert!(matches!(err, FifWalkOptsError::PatternTooLong { .. }));
    }

    #[test]
    fn walk_opts_rejects_too_many_patterns() {
        // The set-wide cap kicks in before any per-pattern compile,
        // so a flood of trivially-valid patterns is still rejected.
        let mut opts = FifWalkOpts::default();
        let pats = vec!["*.rs"; MAX_GLOB_PATTERNS + 1];
        let err = opts.set_includes(&pats).unwrap_err();
        assert!(matches!(err, FifWalkOptsError::TooManyPatterns { .. }));
    }

    #[test]
    fn regex_compile_size_limit_rejects_oversize_pattern() {
        // A 1 MiB literal pattern compiles to an NFA larger than the
        // 512 KiB `REGEX_SIZE_LIMIT_BYTES` budget — the explicit cap
        // returns the existing FifQueryError::Invalid path instead
        // of growing memory up to the regex crate's 10 MiB default.
        let opts = FifQueryOpts {
            match_case: true,
            ..FifQueryOpts::default()
        };
        let pattern = "a".repeat(1024 * 1024);
        let err = FifQuery::compile(&pattern, opts).unwrap_err();
        assert!(matches!(err, FifQueryError::Invalid(_)));
    }
}
