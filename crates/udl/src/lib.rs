//! User Defined Language (UDL) XML parser + in-memory model.
//!
//! Implements Notepad++'s UDL v2.1 XML format so Code++ can load
//! any UDL file that works in N++ unchanged. See DESIGN.md §7.2
//! Phase 4.6 for the phase context and the m1 → m4 milestone
//! breakdown; this crate is Phase 4.6 m1 (**loading + in-memory
//! model only** — the container-lexer tokeniser runtime that
//! consumes an `UdlDefinition` and answers `SCN_STYLENEEDED`
//! notifications lands in a subsequent commit).
//!
//! # Format summary
//!
//! A UDL file is a small XML document with a fixed shape:
//!
//! ```xml
//! <NotepadPlus>
//!   <UserLang name="..." ext="..." udlVersion="2.1" darkModeTheme="yes|no">
//!     <Settings>
//!       <Global caseIgnored="..." allowFoldOfComments="..." foldCompact="..."
//!               forcePureLC="0|1|2" decimalSeparator="0|1|2" />
//!       <Prefix Keywords1="yes|no" ... Keywords8="yes|no" />
//!     </Settings>
//!     <KeywordLists>
//!       <Keywords name="Comments">...</Keywords>
//!       <Keywords name="Numbers, prefix1">...</Keywords>
//!       ... 28 entries total, distinguished by `name`
//!     </KeywordLists>
//!     <Styles>
//!       <WordsStyle name="DEFAULT" fgColor="RRGGBB" bgColor="RRGGBB"
//!                   fontName="..." fontStyle="0..7" nesting="..." />
//!       ... one per named style slot
//!     </Styles>
//!   </UserLang>
//! </NotepadPlus>
//! ```
//!
//! The two shapes that make this NOT a straight serde-derive
//! target are (a) the `<Keywords>` and `<WordsStyle>` elements
//! that all share their tag name but are distinguished by the
//! `name` attribute, and (b) the `yes` / `no` string encoding for
//! booleans. We deserialise via a set of raw-XML shadow structs
//! and then normalise into the public [`UdlDefinition`] shape in
//! [`UdlDefinition::from_raw`]; callers only see the normalised
//! model.
//!
//! # Non-goals (for m1)
//!
//! - **Delimiter-string parsing** — the `Delimiters` and `Comments`
//!   keyword lists use a compact `NN<sequence>` prefix encoding
//!   (e.g. `00#` for a comment-line marker, `02((EOL))` for close-
//!   at-line-end, `03<!--` for a block-comment opener, plus a
//!   backtick-context multi-line closer). This crate stores the
//!   raw strings verbatim; the tokeniser runtime in m1c interprets
//!   them.
//! - **Style-slot → Scintilla mapping** — the m1c runtime maps
//!   each of the 24 named `<WordsStyle>` slots to a Scintilla
//!   style index; this crate just carries the raw name.
//! - **UDL editor round-trip** — the m3 editor modal will need a
//!   serialiser back to the XML format. This crate exposes only
//!   `parse` today; `serialise` lands with m3.

use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use serde::Deserialize;

pub mod registry;
pub mod rules;
pub mod tokenise;
pub use registry::{is_udl_lang_id, UdlEntry, UdlRegistry, UDL_LANG_TYPE_BASE, UDL_LANG_TYPE_END};
pub use rules::{
    CommentRules, DelimiterRule, DelimiterRules, Sequence, MAX_ALTERNATIVES_PER_SLOT,
    MAX_LITERAL_BYTES,
};
pub use tokenise::{StyleEvent, Tokeniser, UdlStyleSlot};

/// Hard cap on the byte-size of a UDL file. **256 KiB.**
///
/// The `userDefineLangs/` directory is user-writable and users
/// routinely drop third-party UDLs from the internet into it; a
/// malicious (or accidentally huge) file needs to fail fast rather
/// than OOM the editor at the first startup scan. Real-world UDLs
/// — including every fixture in `notepad-plus-plus/
/// userDefinedLanguages` and Edditoria's `markdown-plus-plus` —
/// sit in the low-single-digit-KB range, so 256 KiB is roughly
/// 30-100× the actual working set while still small enough that a
/// pathological file trips the cap during read (via a bounded
/// `Read::take` adapter) instead of during allocation. The whole
/// file is loaded into a `String` for `quick_xml::de::from_str`,
/// so this cap directly bounds peak parser memory.
pub const MAX_UDL_FILE_BYTES: u64 = 256 * 1024;

/// Parsed, normalised UDL definition — the public data model. One
/// per `<UserLang>` element in a UDL XML file. A single file
/// contains one `<UserLang>` in practice (every fixture in
/// `notepad-plus-plus/userDefinedLanguages` follows that
/// convention), but the surrounding `<NotepadPlus>` root is what
/// N++ writes and this crate reads.
#[derive(Debug, Clone, PartialEq)]
pub struct UdlDefinition {
    /// User-facing name shown in the Language menu — e.g.
    /// `"Markdown (preinstalled)"`. `name` attribute of the
    /// `<UserLang>` element.
    pub name: String,
    /// File extensions (without the leading dot, lower-cased) that
    /// map to this UDL. Parsed from the space-separated `ext`
    /// attribute — `ext="md markdown"` → `["md", "markdown"]`.
    /// Whitespace-only extensions are skipped defensively.
    pub extensions: Vec<String>,
    /// UDL format version — always `"2.1"` for current N++. Kept
    /// as an opaque string so a future `"2.2"` doesn't force a
    /// parse error before we've decided what to do with it.
    pub udl_version: String,
    /// The `darkModeTheme="yes"` attribute is written by N++ (and
    /// present in Edditoria's dark-mode markdown UDL variants
    /// upstream) to distinguish palettes tuned for dark editor
    /// backgrounds. Code++ ships only the light variant today; a
    /// future commit can bundle a dark counterpart alongside once
    /// Code++'s own dark-mode theme lands. Kept in the model so
    /// UDL files that carry the attribute round-trip losslessly.
    pub dark_mode_theme: bool,
    /// Global lexer settings (case sensitivity, comment folding,
    /// line-comment position, decimal separator).
    pub settings: UdlSettings,
    /// Per-class prefix-mode flags for the 8 keyword classes
    /// (`Keywords1`..=`Keywords8`).
    pub prefix: [bool; 8],
    /// All 28 keyword lists (comments / numbers / operators /
    /// folders / keywords1..=8 / delimiters), keyed by their
    /// N++-canonical `name` attribute.
    pub keyword_lists: UdlKeywordLists,
    /// Named style slots (`DEFAULT`, `COMMENTS`, `KEYWORDS1`, …).
    /// Order preserved from the source XML.
    pub styles: Vec<UdlStyle>,
    /// The path this definition was loaded from, when known.
    /// `Some(path)` for [`UdlDefinition::from_file`], `None` for
    /// in-memory [`UdlDefinition::parse`] calls (tests, or a
    /// future editor modal that hasn't saved yet). Threaded so
    /// the directory scanner (m1b), file-watch integration, and
    /// the editor's "Save" action (m3) don't each have to carry
    /// `(PathBuf, UdlDefinition)` tuples through their own state.
    pub source_path: Option<std::path::PathBuf>,
}

impl UdlDefinition {
    /// Parse a UDL XML file from the given path. Returns the first
    /// `<UserLang>` element in document order — every real-world
    /// UDL file contains exactly one.
    ///
    /// The read is bounded to [`MAX_UDL_FILE_BYTES`] — files larger
    /// than the cap error with [`UdlError::TooLarge`] instead of
    /// being fully loaded. This defends the startup directory scan
    /// against a user-writable `userDefineLangs/` folder that
    /// contains a malicious or accidentally-huge file.
    ///
    /// **Caller responsibility.** No path canonicalisation,
    /// symlink rejection, or containment check happens here — the
    /// caller must restrict `path` to the intended UDL directory.
    /// The m1b directory scanner will (via the platform's
    /// user-config-directory helpers) resolve `path` inside
    /// `<config_dir>/userDefineLangs/` before invoking us, so a
    /// planted symlink outside that directory is out of scope for
    /// this crate.
    ///
    /// # Errors
    ///
    /// - [`UdlError::Io`] — filesystem read failure (path missing,
    ///   permission denied, mid-read I/O error, or read exceeded
    ///   the [`MAX_UDL_FILE_BYTES`] cap — the last is surfaced as
    ///   [`UdlError::TooLarge`], see next).
    /// - [`UdlError::TooLarge`] — file size exceeded the cap.
    /// - [`UdlError::Parse`] — malformed XML.
    /// - [`UdlError::MissingUserLang`] — valid XML with no
    ///   `<UserLang>` element.
    pub fn from_file(path: &Path) -> Result<Self, UdlError> {
        let file = std::fs::File::open(path).map_err(|source| UdlError::Io {
            path: path.to_owned(),
            source,
        })?;
        // `take` bounds the number of bytes the `Read` impl will
        // yield. We ask for one byte past the cap so a file of
        // exactly `MAX_UDL_FILE_BYTES` bytes still fits, and a
        // file one byte larger yields exactly `MAX+1` bytes —
        // which we check below to distinguish "fits under cap"
        // from "hit cap and truncated". Doing this via `take`
        // rather than a `metadata().len()` pre-check avoids a
        // TOCTOU window where the file could grow between the
        // metadata call and the actual read.
        let mut reader = file.take(MAX_UDL_FILE_BYTES + 1);
        let mut contents = String::new();
        reader
            .read_to_string(&mut contents)
            .map_err(|source| UdlError::Io {
                path: path.to_owned(),
                source,
            })?;
        if contents.len() as u64 > MAX_UDL_FILE_BYTES {
            return Err(UdlError::TooLarge {
                path: path.to_owned(),
                cap: MAX_UDL_FILE_BYTES,
            });
        }
        let mut udl = Self::parse(&contents)?;
        udl.source_path = Some(path.to_owned());
        Ok(udl)
    }

    /// Parse a UDL XML document from a string. See
    /// [`UdlDefinition::from_file`] for the file-loading variant.
    ///
    /// Deliberately named `parse` (rather than `from_str`) to keep
    /// the shape distinct from `std::str::FromStr::from_str` — the
    /// trait's `Err` type would have to be constructible without
    /// context, but our [`UdlError::Io`] carries a `PathBuf` that
    /// only makes sense on the file-loading path.
    ///
    /// # Errors
    ///
    /// Returns [`UdlError::Parse`] if the XML doesn't deserialise
    /// or [`UdlError::MissingUserLang`] if it contains no
    /// `<UserLang>` element.
    pub fn parse(xml: &str) -> Result<Self, UdlError> {
        let root: RawNotepadPlus = quick_xml::de::from_str(xml).map_err(UdlError::Parse)?;
        let count = root.user_langs.len();
        let user_lang = root
            .user_langs
            .into_iter()
            .next()
            .ok_or(UdlError::MissingUserLang)?;
        if count > 1 {
            // Every real-world UDL file contains exactly one
            // `<UserLang>` (both N++'s own writer and every fixture
            // in the notepad-plus-plus/userDefinedLanguages
            // collection follow that convention). Log the drop so
            // a user hand-editing a multi-lang file has a
            // diagnostic trail rather than silent data loss.
            //
            // `?kept` (Debug) rather than the bare shorthand so a
            // hostile UDL name containing terminal escape sequences
            // renders as escaped `\u{...}` rather than being
            // written raw to whatever sink the tracing subscriber
            // wires up. Same treatment applied to every other
            // attacker-controlled string in this file.
            tracing::warn!(
                dropped = count - 1,
                kept = ?user_lang.name,
                "UDL XML contained more than one <UserLang> element; \
                 keeping the first, dropping the rest"
            );
        }
        Ok(Self::from_raw(user_lang))
    }

    /// Normalise a raw-XML `<UserLang>` element into the public
    /// [`UdlDefinition`] shape. Splits the space-separated
    /// `extensions` string, translates `yes`/`no` booleans, and
    /// maps the flat `<Keywords>` list to
    /// [`UdlKeywordLists`]'s named fields.
    fn from_raw(raw: RawUserLang) -> Self {
        let extensions: Vec<String> = raw
            .ext
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();

        let settings = UdlSettings {
            case_ignored: raw.settings.global.case_ignored.into(),
            allow_fold_of_comments: raw.settings.global.allow_fold_of_comments.into(),
            fold_compact: raw.settings.global.fold_compact.into(),
            force_pure_lc: raw.settings.global.force_pure_lc,
            decimal_separator: raw.settings.global.decimal_separator,
        };

        let prefix = [
            raw.settings.prefix.keywords1.into(),
            raw.settings.prefix.keywords2.into(),
            raw.settings.prefix.keywords3.into(),
            raw.settings.prefix.keywords4.into(),
            raw.settings.prefix.keywords5.into(),
            raw.settings.prefix.keywords6.into(),
            raw.settings.prefix.keywords7.into(),
            raw.settings.prefix.keywords8.into(),
        ];

        let keyword_lists = UdlKeywordLists::from_raw(raw.keyword_lists.keywords);

        let styles: Vec<UdlStyle> = raw
            .styles
            .words_styles
            .into_iter()
            .map(UdlStyle::from_raw)
            .collect();

        Self {
            name: raw.name,
            extensions,
            udl_version: raw.udl_version,
            dark_mode_theme: raw.dark_mode_theme.is_some_and(bool::from),
            settings,
            prefix,
            keyword_lists,
            styles,
            source_path: None,
        }
    }
}

/// Global lexer settings — the `<Global>` element inside
/// `<Settings>`. See the N++ UDL editor's "Folder & Default" tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdlSettings {
    /// `caseIgnored="yes"` — keyword matching is case-insensitive.
    /// Applies to every `<Keywords>` list including numbers /
    /// operators / delimiters.
    pub case_ignored: bool,
    /// `allowFoldOfComments="yes"` — Scintilla's folder considers
    /// comment blocks foldable.
    pub allow_fold_of_comments: bool,
    /// `foldCompact="yes"` — fold empty lines into the enclosing
    /// block rather than leaving them at the outer level. Mirrors
    /// Scintilla's `fold.compact` property.
    pub fold_compact: bool,
    /// Line-comment position policy per the N++ editor's radio
    /// group: `0` = allow anywhere on a line (comment mark can
    /// follow code), `1` = force at beginning of line, `2` = allow
    /// preceding whitespace only. Default per the markdown fixture
    /// is `2`.
    pub force_pure_lc: u8,
    /// Decimal-separator handling for numeric literals: `0` = dot
    /// only, `1` = comma only, `2` = both accepted.
    pub decimal_separator: u8,
}

/// All 28 keyword lists carried by a UDL file. `name`-attribute-
/// keyed within the source XML; unified here into a struct with
/// documented, N++-canonical field names.
///
/// Fields are all `String` — the compact `NN<seq>` prefix encoding
/// used by `comments` and `delimiters` is preserved verbatim and
/// interpreted by the tokeniser runtime (Phase 4.6 m1c). Empty
/// strings are the "no entry" case (e.g. markdown's
/// `keywords8: ""`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UdlKeywordLists {
    /// Comment-mark encoding: `00<line-mark> 01<continue-char>
    /// 02((EOL)) 03<block-open> 04<block-close>`. Markdown fixture:
    /// `"00# 01 02((EOL)) 03<!-- 04-->"`.
    pub comments: String,
    /// Numeric-literal prefix set 1 — characters/tokens that may
    /// precede a number (e.g. `$` for hex).
    pub numbers_prefix1: String,
    /// Numeric-literal prefix set 2 — an alternate prefix family.
    pub numbers_prefix2: String,
    /// Numeric-literal extras set 1 — characters that may appear
    /// inside a number (e.g. `_` in Rust's `1_000`).
    pub numbers_extras1: String,
    /// Numeric-literal extras set 2 — alternate extras family.
    pub numbers_extras2: String,
    /// Numeric-literal suffix set 1 — trailing characters after a
    /// number (e.g. `L` for a long literal).
    pub numbers_suffix1: String,
    /// Numeric-literal suffix set 2 — alternate suffix family.
    pub numbers_suffix2: String,
    /// Numeric-literal range operator — e.g. `..` for Rust.
    pub numbers_range: String,
    /// Operators list 1 — no whitespace required between tokens.
    /// The markdown fixture uses this for the escape sequences
    /// (`\<`, `\>`, `\*`, `\_`, …) and Markdown-table pipe
    /// tokens.
    pub operators1: String,
    /// Operators list 2 — whitespace-delimited operators (per the
    /// UDL editor's "Operators 2 (separators required)" label).
    pub operators2: String,
    /// Folder marker: code-block 1 opener sequence.
    pub folders_in_code1_open: String,
    /// Folder marker: code-block 1 middle sequence.
    pub folders_in_code1_middle: String,
    /// Folder marker: code-block 1 closer sequence.
    pub folders_in_code1_close: String,
    /// Folder marker: code-block 2 opener sequence.
    pub folders_in_code2_open: String,
    /// Folder marker: code-block 2 middle sequence.
    pub folders_in_code2_middle: String,
    /// Folder marker: code-block 2 closer sequence.
    pub folders_in_code2_close: String,
    /// Folder marker: comment-block opener sequence.
    pub folders_in_comment_open: String,
    /// Folder marker: comment-block middle sequence.
    pub folders_in_comment_middle: String,
    /// Folder marker: comment-block closer sequence.
    pub folders_in_comment_close: String,
    /// Eight user-defined keyword classes, indexed 0..=7. Each
    /// class maps to a distinct style slot (`KEYWORDS1`..=`8`) and
    /// gets its own foreground/background/font/bold/italic
    /// configuration via [`UdlStyle`]. Prefix-mode flag lives on
    /// [`UdlDefinition::prefix`].
    pub keywords: [String; 8],
    /// Delimiter-pair encoding: 8 slots × 3 sub-parts (open /
    /// escape / close) → 24 numbered prefixes `00..=23`. The
    /// Markdown fixture begins with square-bracket-image / plain
    /// square-bracket openers (both keyed as `00`) followed by a
    /// backslash escape (`01`) and matching closers (`02`); further
    /// entries cover triple-backtick and tilde code fences,
    /// bold/italic delimiters, etc.
    pub delimiters: String,
}

impl UdlKeywordLists {
    /// Populate the named fields from the flat list of
    /// `<Keywords name="...">` entries. Unknown `name` values are
    /// logged via `tracing` and skipped rather than failing —
    /// forward compatibility with a hypothetical future N++
    /// adding new lists shouldn't break existing UDL loading.
    /// Known lists absent from the file are left at their default
    /// empty-string value.
    ///
    /// **v2.0-style aliases.** Real-world UDLs shipping with
    /// `udlVersion="2.0"` (observed on Luis Pisco's
    /// `Cisco_IOS_byLuisPisco.xml`, among others in the
    /// `notepad-plus-plus/userDefinedLanguages` collection)
    /// use these four number-list names verbatim, distinct
    /// from the `"Numbers, prefix1"` / `"Numbers, extras1"` /
    /// `"Numbers, suffix1"` / `"Numbers, extras2"` set our
    /// preinstalled Markdown v2.1 fixture uses. Whether N++
    /// formally renamed these between UDL v2.0 and v2.1 or
    /// accepts both forms in parallel isn't established from
    /// N++'s public docs — the pragmatic observation is that
    /// they appear in real files a user drops into
    /// `userDefineLangs/`, and without recognising them the
    /// scanner's `tracing::warn!("unknown …; skipped")` branch
    /// fires and number-literal highlighting silently drops.
    /// Accepting them as aliases for the closest-semantics
    /// v2.1 slot is a compat best-effort — the mapping isn't
    /// authoritative (`"Numbers, additional"` → `extras2` in
    /// particular is a judgment call), but the failure mode is
    /// bounded: a mis-assigned number slot yields slightly
    /// different colouring than N++ would show for the same
    /// file, not a crash or a load failure.
    fn from_raw(entries: Vec<RawKeywords>) -> Self {
        let mut lists = Self::default();
        for entry in entries {
            match entry.name.as_str() {
                "Comments" => lists.comments = entry.value,
                // Canonical (v2.1-fixture) name paired with the
                // v2.0-style alias observed in real N++
                // community UDLs. Aliased arms map to the
                // closest-semantics v2.1 slot; see the
                // docstring above for the compat-best-effort
                // rationale.
                "Numbers, prefix1" | "Numbers, prefixes" => {
                    lists.numbers_prefix1 = entry.value;
                }
                "Numbers, prefix2" => lists.numbers_prefix2 = entry.value,
                "Numbers, extras1" | "Numbers, extras with prefixes" => {
                    lists.numbers_extras1 = entry.value;
                }
                "Numbers, extras2" | "Numbers, additional" => {
                    lists.numbers_extras2 = entry.value;
                }
                "Numbers, suffix1" | "Numbers, suffixes" => {
                    lists.numbers_suffix1 = entry.value;
                }
                "Numbers, suffix2" => lists.numbers_suffix2 = entry.value,
                "Numbers, range" => lists.numbers_range = entry.value,
                "Operators1" => lists.operators1 = entry.value,
                "Operators2" => lists.operators2 = entry.value,
                "Folders in code1, open" => lists.folders_in_code1_open = entry.value,
                "Folders in code1, middle" => lists.folders_in_code1_middle = entry.value,
                "Folders in code1, close" => lists.folders_in_code1_close = entry.value,
                "Folders in code2, open" => lists.folders_in_code2_open = entry.value,
                "Folders in code2, middle" => lists.folders_in_code2_middle = entry.value,
                "Folders in code2, close" => lists.folders_in_code2_close = entry.value,
                "Folders in comment, open" => lists.folders_in_comment_open = entry.value,
                "Folders in comment, middle" => lists.folders_in_comment_middle = entry.value,
                "Folders in comment, close" => lists.folders_in_comment_close = entry.value,
                "Keywords1" => lists.keywords[0] = entry.value,
                "Keywords2" => lists.keywords[1] = entry.value,
                "Keywords3" => lists.keywords[2] = entry.value,
                "Keywords4" => lists.keywords[3] = entry.value,
                "Keywords5" => lists.keywords[4] = entry.value,
                "Keywords6" => lists.keywords[5] = entry.value,
                "Keywords7" => lists.keywords[6] = entry.value,
                "Keywords8" => lists.keywords[7] = entry.value,
                "Delimiters" => lists.delimiters = entry.value,
                unknown => {
                    // `?name` (Debug) escapes control characters
                    // — a hostile UDL crafting an entry name with
                    // ANSI escapes shouldn't be able to inject
                    // sequences into whatever log sink is wired
                    // up. Same defense as the multi-UserLang
                    // warning above.
                    tracing::warn!(
                        name = ?unknown,
                        "unknown UDL <Keywords name=...> entry; skipped"
                    );
                }
            }
        }
        lists
    }
}

/// One named style slot — the `<WordsStyle>` element. Every UDL
/// file carries 24 of these (`DEFAULT`, `COMMENTS`, `LINE COMMENTS`,
/// `NUMBERS`, `KEYWORDS1`..=`8`, `OPERATORS`, `FOLDER IN CODE1`,
/// `FOLDER IN CODE2`, `FOLDER IN COMMENT`, `DELIMITERS1`..=`8`).
/// The tokeniser runtime (m1c) maps each named slot to a Scintilla
/// style index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdlStyle {
    /// Slot name — e.g. `"KEYWORDS1"`. See [`UdlStyle`]'s type-
    /// level doc for the full set of names.
    pub name: String,
    /// Foreground colour as a 24-bit RGB integer parsed from the
    /// `fgColor="RRGGBB"` hex attribute — `RRGGBB` here means byte
    /// order red-most-significant, i.e. `0x8000FF` is
    /// `R=0x80 G=0x00 B=0xFF`. **NOT** the Scintilla `COLORREF`
    /// (0x00BBGGRR) format; the runtime converts between them
    /// when applying to Scintilla. Parse failure fields at 0
    /// (black).
    pub fg_color: u32,
    /// Background colour, same RGB encoding as [`Self::fg_color`].
    pub bg_color: u32,
    /// Font-family name — empty string means "inherit from the
    /// UDL's DEFAULT style" per N++ convention.
    pub font_name: String,
    /// Font-style bitfield: `1` = bold, `2` = italic, `4` =
    /// underline. Combined by summing — `3` is bold+italic.
    pub font_style: u8,
    /// Nesting bitfield — which style slots can nest inside this
    /// one. Bit N corresponds to slot number N in the N++ nesting
    /// dialog. The markdown fixture uses `65600` (0x10040) on
    /// `DELIMITERS4` and `32800` (0x8020) on `DELIMITERS5` to
    /// allow specific inner delimiters. See the tokeniser runtime
    /// (m1c) for the bit-index → slot-name mapping.
    pub nesting: u32,
}

impl UdlStyle {
    fn from_raw(raw: RawWordsStyle) -> Self {
        Self {
            name: raw.name,
            fg_color: parse_hex_color(&raw.fg_color),
            bg_color: parse_hex_color(&raw.bg_color),
            font_name: raw.font_name,
            font_style: raw.font_style,
            nesting: raw.nesting,
        }
    }
}

/// Parse a `RRGGBB` hex colour string into a 24-bit integer.
/// Malformed strings resolve to `0` (black) with a `tracing::warn`
/// so a broken UDL still loads with a visible fallback rather
/// than failing the whole file. N++ writes 6-hex-digit uppercase
/// values without a `#` prefix; we accept both cases.
fn parse_hex_color(s: &str) -> u32 {
    let trimmed = s.strip_prefix('#').unwrap_or(s);
    match u32::from_str_radix(trimmed, 16) {
        Ok(value) => value & 0x00FF_FFFF,
        Err(err) => {
            // `?value` (Debug) escapes control characters — a
            // hostile UDL crafting a colour attribute with ANSI
            // escapes shouldn't be able to inject sequences into
            // the log sink. Same defense as the other tracing
            // calls in this file.
            tracing::warn!(
                value = ?s,
                error = %err,
                "malformed UDL hex colour; defaulting to 0 (black)"
            );
            0
        }
    }
}

/// UDL parse / load errors. `Io` covers filesystem failures on
/// [`UdlDefinition::from_file`]; `Parse` covers XML syntax errors;
/// `MissingUserLang` covers a well-formed XML document that
/// doesn't contain any `<UserLang>` element (empty
/// `<NotepadPlus>`).
///
/// `#[non_exhaustive]` so subsequent milestones (m1b directory
/// scan, m3 editor save) can add variants (e.g. duplicate-name
/// conflict when merging multiple files) without breaking every
/// caller's `match`.
#[derive(Debug)]
#[non_exhaustive]
pub enum UdlError {
    /// Filesystem read error. Includes the path so a startup-time
    /// UDL scan can log which specific file failed without the
    /// caller having to thread it through.
    Io {
        /// The path that failed to read.
        path: std::path::PathBuf,
        /// Underlying io error.
        source: std::io::Error,
    },
    /// File size exceeded [`MAX_UDL_FILE_BYTES`]. Distinct from
    /// [`Self::Io`] so the m1b startup scanner can log an
    /// unambiguous "skipped: too large" diagnostic and continue
    /// with the rest of the directory rather than treating the
    /// oversized file as a generic IO failure.
    TooLarge {
        /// The path whose size exceeded the cap.
        path: std::path::PathBuf,
        /// The active [`MAX_UDL_FILE_BYTES`] value at the time of
        /// the read. Recorded on the error so a future cap change
        /// leaves the log message self-consistent.
        cap: u64,
    },
    /// XML deserialisation error — malformed markup, missing
    /// required attribute, or unexpected element shape.
    Parse(quick_xml::DeError),
    /// The document was valid XML but contained no `<UserLang>`
    /// element inside `<NotepadPlus>`.
    MissingUserLang,
}

impl std::fmt::Display for UdlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "read {}: {source}", path.display())
            }
            Self::TooLarge { path, cap } => {
                write!(f, "UDL file {} exceeds the {cap}-byte cap", path.display())
            }
            Self::Parse(err) => write!(f, "parse UDL XML: {err}"),
            Self::MissingUserLang => f.write_str("UDL XML contained no <UserLang> element"),
        }
    }
}

impl std::error::Error for UdlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(err) => Some(err),
            Self::TooLarge { .. } | Self::MissingUserLang => None,
        }
    }
}

// -------------------------------------------------------------
// Raw-XML shadow structs — mirror the on-disk schema 1:1 for
// serde deserialisation, then get normalised into the public
// types above via `UdlDefinition::from_raw`. Kept crate-private
// so callers never see the yes/no strings or the flat-list
// keyword layout.
// -------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename = "NotepadPlus")]
struct RawNotepadPlus {
    #[serde(rename = "UserLang", default)]
    user_langs: Vec<RawUserLang>,
}

#[derive(Debug, Deserialize)]
struct RawUserLang {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@ext")]
    ext: String,
    #[serde(rename = "@udlVersion")]
    udl_version: String,
    /// Optional: only present in dark-mode variants of the
    /// preinstalled markdown UDL. `None` on light variants.
    #[serde(rename = "@darkModeTheme", default)]
    dark_mode_theme: Option<YesNo>,
    #[serde(rename = "Settings")]
    settings: RawSettings,
    #[serde(rename = "KeywordLists")]
    keyword_lists: RawKeywordLists,
    #[serde(rename = "Styles")]
    styles: RawStyles,
}

#[derive(Debug, Deserialize)]
struct RawSettings {
    #[serde(rename = "Global")]
    global: RawGlobal,
    #[serde(rename = "Prefix")]
    prefix: RawPrefix,
}

/// `<Global>` attributes. All fields are `#[serde(default)]` so
/// pre-v2.1 UDL files (which omit `@decimalSeparator`, added in
/// v2.1) load without error. N++'s implicit defaults are the
/// `Default` of each field's type — matches the tolerance N++'s
/// own loader shows for hand-edited or older UDLs.
#[derive(Debug, Deserialize)]
struct RawGlobal {
    #[serde(rename = "@caseIgnored", default)]
    case_ignored: YesNo,
    #[serde(rename = "@allowFoldOfComments", default)]
    allow_fold_of_comments: YesNo,
    #[serde(rename = "@foldCompact", default)]
    fold_compact: YesNo,
    #[serde(rename = "@forcePureLC", default)]
    force_pure_lc: u8,
    /// v2.1 addition; absent on v2.0 files → default `0` (dot).
    #[serde(rename = "@decimalSeparator", default)]
    decimal_separator: u8,
}

/// `<Prefix>` attributes. All fields are `#[serde(default)]`
/// so a hand-edited UDL missing one of the eight `KeywordsN`
/// attributes defaults it to `no` (prefix-mode off) rather than
/// erroring out — same tolerance N++ shows for older or partial
/// UDLs.
#[derive(Debug, Deserialize)]
struct RawPrefix {
    #[serde(rename = "@Keywords1", default)]
    keywords1: YesNo,
    #[serde(rename = "@Keywords2", default)]
    keywords2: YesNo,
    #[serde(rename = "@Keywords3", default)]
    keywords3: YesNo,
    #[serde(rename = "@Keywords4", default)]
    keywords4: YesNo,
    #[serde(rename = "@Keywords5", default)]
    keywords5: YesNo,
    #[serde(rename = "@Keywords6", default)]
    keywords6: YesNo,
    #[serde(rename = "@Keywords7", default)]
    keywords7: YesNo,
    #[serde(rename = "@Keywords8", default)]
    keywords8: YesNo,
}

#[derive(Debug, Deserialize)]
struct RawKeywordLists {
    #[serde(rename = "Keywords", default)]
    keywords: Vec<RawKeywords>,
}

#[derive(Debug, Deserialize)]
struct RawKeywords {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "$text", default)]
    value: String,
}

#[derive(Debug, Deserialize)]
struct RawStyles {
    #[serde(rename = "WordsStyle", default)]
    words_styles: Vec<RawWordsStyle>,
}

#[derive(Debug, Deserialize)]
struct RawWordsStyle {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@fgColor")]
    fg_color: String,
    #[serde(rename = "@bgColor")]
    bg_color: String,
    #[serde(rename = "@fontName", default)]
    font_name: String,
    #[serde(rename = "@fontStyle")]
    font_style: u8,
    #[serde(rename = "@nesting", default)]
    nesting: u32,
}

/// Yes/no boolean encoding used by every UDL attribute. Custom
/// `Deserialize` impl accepts the exact strings N++ writes plus
/// tolerant variants (case-insensitive `"yes"`/`"no"`/`"true"`/
/// `"false"`/`"1"`/`"0"`) — the tolerance protects against
/// hand-edits by users who don't know the strict format.
///
/// `Default` is `YesNo(false)` — used by `#[serde(default)]` on
/// `<Global>`/`<Prefix>` attributes that were added post-v2.0
/// so pre-v2.1 UDL files (like the community collection's
/// `Cisco_IOS_byLuisPisco.xml`) load with N++'s implicit
/// defaults rather than erroring out.
#[derive(Debug, Clone, Copy, Default)]
struct YesNo(bool);

impl From<YesNo> for bool {
    fn from(y: YesNo) -> Self {
        y.0
    }
}

impl FromStr for YesNo {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "yes" | "true" | "1" => Ok(Self(true)),
            "no" | "false" | "0" => Ok(Self(false)),
            other => Err(format!(
                "expected yes/no (or true/false, 1/0); got {other:?}"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for YesNo {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to the preinstalled markdown UDL fixture (checked in
    /// under `assets/preinstalled-udls/` at the workspace root).
    /// Resolved relative to `CARGO_MANIFEST_DIR` so tests work
    /// from any working directory.
    fn markdown_fixture_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("preinstalled-udls")
            .join("markdown._preinstalled.udl.xml")
    }

    #[test]
    fn parses_markdown_preinstalled_fixture() {
        let path = markdown_fixture_path();
        let udl = UdlDefinition::from_file(&path).expect("fixture must parse cleanly");

        assert_eq!(udl.name, "Markdown (preinstalled)");
        assert_eq!(udl.extensions, vec!["md".to_owned(), "markdown".to_owned()]);
        assert_eq!(udl.udl_version, "2.1");
        assert!(
            !udl.dark_mode_theme,
            "light variant must NOT carry darkModeTheme=yes"
        );
    }

    #[test]
    fn markdown_settings_match_fixture_values() {
        let udl =
            UdlDefinition::from_file(&markdown_fixture_path()).expect("fixture must parse cleanly");

        assert!(udl.settings.case_ignored);
        assert!(!udl.settings.allow_fold_of_comments);
        assert!(!udl.settings.fold_compact);
        assert_eq!(
            udl.settings.force_pure_lc, 2,
            "markdown fixture uses `2` = allow preceding whitespace"
        );
        assert_eq!(
            udl.settings.decimal_separator, 0,
            "markdown fixture uses `0` = dot only"
        );
    }

    #[test]
    fn markdown_prefix_flags_match_fixture() {
        let udl =
            UdlDefinition::from_file(&markdown_fixture_path()).expect("fixture must parse cleanly");

        // Fixture: Keywords1..=7 all "yes", Keywords8 "no".
        assert_eq!(
            udl.prefix,
            [true, true, true, true, true, true, true, false]
        );
    }

    #[test]
    fn markdown_keyword_lists_populated() {
        let udl =
            UdlDefinition::from_file(&markdown_fixture_path()).expect("fixture must parse cleanly");
        let kw = &udl.keyword_lists;

        // Comment-mark encoding preserved verbatim (no interpretation
        // yet — that's m1c's job).
        assert_eq!(kw.comments, "00# 01 02((EOL)) 03<!-- 04-->");

        // Fixture's Keywords1 covers URL-like prefixes.
        assert!(kw.keywords[0].contains("http://"));
        assert!(kw.keywords[0].contains("mailto:"));
        // Keywords2 is Markdown's setext-heading markers.
        assert!(kw.keywords[1].contains("===="));
        assert!(kw.keywords[1].contains("----"));
        // Keywords8 is empty in the fixture.
        assert!(kw.keywords[7].is_empty());

        // Delimiter encoding — 24 numbered slots, verbatim.
        assert!(kw.delimiters.starts_with("00!["));
        assert!(kw.delimiters.contains("((EOL"));

        // Numbers,suffix1/2 both `.` in the fixture; ranges empty.
        assert_eq!(kw.numbers_suffix1, ".");
        assert_eq!(kw.numbers_suffix2, ".");
        assert!(kw.numbers_range.is_empty());
    }

    #[test]
    fn markdown_styles_match_fixture() {
        let udl =
            UdlDefinition::from_file(&markdown_fixture_path()).expect("fixture must parse cleanly");

        // 24 named style slots per the fixture; count-pin so a
        // silent structural drift is caught.
        assert_eq!(udl.styles.len(), 24);

        // Spot-check a handful of specific slots against the
        // fixture's colours.
        let default_style = udl
            .styles
            .iter()
            .find(|s| s.name == "DEFAULT")
            .expect("DEFAULT must be present");
        // fixture: fgColor="333333"
        assert_eq!(default_style.fg_color, 0x0033_3333);
        // fixture: bgColor="FFFFFF"
        assert_eq!(default_style.bg_color, 0x00FF_FFFF);
        // fixture: fontStyle="0"
        assert_eq!(default_style.font_style, 0);
        assert_eq!(default_style.nesting, 0);

        // COMMENTS has italic (fontStyle=2) and mid-grey (808080).
        let comments = udl
            .styles
            .iter()
            .find(|s| s.name == "COMMENTS")
            .expect("COMMENTS must be present");
        assert_eq!(comments.font_style, 2);
        assert_eq!(comments.fg_color, 0x0080_8080);

        // DELIMITERS4 has nesting=65600 (0x10040) — pinned as-is
        // for the m1c runtime to consume later.
        let delim4 = udl
            .styles
            .iter()
            .find(|s| s.name == "DELIMITERS4")
            .expect("DELIMITERS4 must be present");
        assert_eq!(delim4.nesting, 65_600);
    }

    #[test]
    fn from_file_populates_source_path_but_parse_leaves_it_none() {
        // `from_file` must stamp `source_path` so the m1b directory
        // scanner and the m3 editor modal can each know where a UDL
        // came from without threading the path separately. `parse`
        // (string entry point) has no path context — must be None.
        let path = markdown_fixture_path();
        let from_file = UdlDefinition::from_file(&path).expect("fixture must parse");
        assert_eq!(
            from_file.source_path.as_deref(),
            Some(path.as_path()),
            "from_file must stamp the source path"
        );

        let contents = std::fs::read_to_string(&path).expect("fixture must be readable");
        let from_str = UdlDefinition::parse(&contents).expect("string parse must succeed");
        assert!(
            from_str.source_path.is_none(),
            "parse has no path context and must leave source_path = None"
        );
    }

    #[test]
    fn from_file_rejects_files_exceeding_max_udl_file_bytes() {
        // The 256-KiB cap defends the startup directory scan
        // against oversized files in the user-writable
        // `userDefineLangs/` folder. Write a file 1 byte past the
        // cap into a tempfile and confirm we get `TooLarge` with
        // the path preserved.
        let tmpdir = std::env::temp_dir();
        let path = tmpdir.join(format!(
            "codepp-udl-oversize-{}.udl.xml",
            std::process::id()
        ));
        let payload_len = usize::try_from(MAX_UDL_FILE_BYTES + 1)
            .expect("cap fits in usize on all supported targets");
        let payload = vec![b' '; payload_len];
        std::fs::write(&path, &payload).expect("tempfile write must succeed");
        let err = UdlDefinition::from_file(&path);
        // Always clean up even if the assertion fails.
        let _ = std::fs::remove_file(&path);
        let err = err.expect_err("oversize file must error");
        match err {
            UdlError::TooLarge {
                path: got_path,
                cap,
            } => {
                assert_eq!(got_path, path);
                assert_eq!(cap, MAX_UDL_FILE_BYTES);
            }
            other => panic!("expected UdlError::TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn dtd_entity_expansion_is_not_supported() {
        // quick-xml's serde deserializer does NOT expand
        // user-defined XML entities from a DOCTYPE internal
        // subset — attempting to reference one is a parse error
        // rather than an entity-expansion attack surface. Pin
        // that behaviour so a future dependency bump/feature
        // addition that changes it doesn't silently open an XXE
        // hole in Code++.
        let xml = r#"<?xml version="1.0"?>
            <!DOCTYPE NotepadPlus [
              <!ENTITY xxe "malicious">
            ]>
            <NotepadPlus>
              <UserLang name="Attack &xxe;" ext="xxe" udlVersion="2.1">
                <Settings>
                  <Global caseIgnored="no" allowFoldOfComments="no"
                          foldCompact="no" forcePureLC="0"
                          decimalSeparator="0" />
                  <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                          Keywords4="no" Keywords5="no" Keywords6="no"
                          Keywords7="no" Keywords8="no" />
                </Settings>
                <KeywordLists />
                <Styles />
              </UserLang>
            </NotepadPlus>"#;
        let err = UdlDefinition::parse(xml).expect_err(
            "user-defined DOCTYPE entities must NOT be expanded — \
             this is Code++'s XXE-prevention regression pin",
        );
        assert!(matches!(err, UdlError::Parse(_)));
    }

    #[test]
    fn from_file_on_missing_path_returns_io_error_with_path() {
        // The m1b directory-scanner uses `UdlError::Io` for the
        // per-file skip-and-log behavior. Pin both the variant and
        // that the offending path is preserved so the scanner's
        // log line has the diagnostic info without re-plumbing.
        let missing = std::path::Path::new("does/not/exist.udl.xml");
        let err = UdlDefinition::from_file(missing).expect_err("missing path must error");
        match err {
            UdlError::Io { path, .. } => {
                assert_eq!(path, missing);
            }
            other => panic!("expected UdlError::Io, got {other:?}"),
        }
    }

    #[test]
    fn missing_user_lang_element_errors() {
        let xml = "<NotepadPlus></NotepadPlus>";
        let err = UdlDefinition::parse(xml).expect_err("empty root must error");
        assert!(matches!(err, UdlError::MissingUserLang));
    }

    #[test]
    fn malformed_xml_errors() {
        let xml = "<NotepadPlus><UserLang>unterminated";
        let err = UdlDefinition::parse(xml).expect_err("malformed XML must error");
        assert!(matches!(err, UdlError::Parse(_)));
    }

    #[test]
    fn tolerates_hand_edited_yes_no_variants() {
        // Users hand-editing UDLs may write `Yes` (capital Y) or
        // `TRUE` — the tolerant `YesNo` parser accepts every
        // documented variant. Build a minimal UDL that exercises
        // the mixed forms and confirm it round-trips.
        let xml = r#"<NotepadPlus>
            <UserLang name="Tolerance" ext="tol" udlVersion="2.1">
              <Settings>
                <Global caseIgnored="YES" allowFoldOfComments="0"
                        foldCompact="1" forcePureLC="0" decimalSeparator="0" />
                <Prefix Keywords1="TRUE" Keywords2="False" Keywords3="0"
                        Keywords4="1" Keywords5="yes" Keywords6="no"
                        Keywords7="yes" Keywords8="no" />
              </Settings>
              <KeywordLists />
              <Styles />
            </UserLang>
          </NotepadPlus>"#;
        let udl = UdlDefinition::parse(xml).expect("tolerant parse must succeed");
        assert!(udl.settings.case_ignored);
        assert!(!udl.settings.allow_fold_of_comments);
        assert!(udl.settings.fold_compact);
        assert_eq!(
            udl.prefix,
            [true, false, false, true, true, false, true, false]
        );
    }

    #[test]
    fn malformed_hex_color_defaults_to_black() {
        assert_eq!(parse_hex_color(""), 0);
        assert_eq!(parse_hex_color("XYZ"), 0);
        assert_eq!(parse_hex_color("FF00FF"), 0x00FF_00FF);
        assert_eq!(parse_hex_color("#00FF00"), 0x0000_FF00);
    }

    #[test]
    fn v2_0_global_without_decimal_separator_parses() {
        // Regression pin: community UDLs like Luis Pisco's
        // `Cisco_IOS_byLuisPisco.xml` are UDL v2.0 and omit the
        // `@decimalSeparator` attribute that v2.1 added. Before
        // this fix, `RawGlobal` hard-required the attribute and
        // the whole file failed to parse — the user would see
        // no Language-menu entry with no obvious reason why.
        // Confirm that `<Global>` without `decimalSeparator`
        // parses cleanly, defaulting the field to `0`.
        let xml = r#"<NotepadPlus>
            <UserLang name="v2.0 Sample" ext="v20" udlVersion="2.0">
              <Settings>
                <Global caseIgnored="yes" allowFoldOfComments="no"
                        forcePureLC="0" foldCompact="no" />
                <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                        Keywords4="no" Keywords5="no" Keywords6="no"
                        Keywords7="no" Keywords8="no" />
              </Settings>
              <KeywordLists />
              <Styles />
            </UserLang>
          </NotepadPlus>"#;
        let udl = UdlDefinition::parse(xml).expect("v2.0 <Global> must parse");
        assert_eq!(udl.udl_version, "2.0");
        assert!(udl.settings.case_ignored);
        assert!(!udl.settings.fold_compact);
        assert_eq!(
            udl.settings.decimal_separator, 0,
            "missing @decimalSeparator must default to 0 (dot)"
        );
    }

    #[test]
    fn v2_0_keyword_names_alias_to_v2_1_slots() {
        // Regression pin: real-world UDLs shipping with
        // `udlVersion="2.0"` use these older number-list names
        // verbatim; without aliasing they'd be dropped as
        // "unknown" and number-literal highlighting would be
        // lost. Whether N++ formally renamed the lists between
        // v2.0 and v2.1 or accepts both forms in parallel
        // isn't established from N++'s public docs — see the
        // hedged docstring on `UdlKeywordLists::from_raw`
        // above for the compat-best-effort rationale.
        let xml = r#"<NotepadPlus>
            <UserLang name="v2.0 Numbers" ext="v20n" udlVersion="2.0">
              <Settings>
                <Global caseIgnored="no" allowFoldOfComments="no"
                        foldCompact="no" forcePureLC="0" />
                <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                        Keywords4="no" Keywords5="no" Keywords6="no"
                        Keywords7="no" Keywords8="no" />
              </Settings>
              <KeywordLists>
                <Keywords name="Numbers, prefixes">$ 0x</Keywords>
                <Keywords name="Numbers, extras with prefixes">_</Keywords>
                <Keywords name="Numbers, suffixes">L UL</Keywords>
                <Keywords name="Numbers, additional">.</Keywords>
              </KeywordLists>
              <Styles />
            </UserLang>
          </NotepadPlus>"#;
        let udl = UdlDefinition::parse(xml).expect("v2.0 keyword aliases must parse");
        // v2.0 name → v2.1 field mapping (see UdlKeywordLists::from_raw
        // docstring for the semantic-closest justification).
        assert_eq!(udl.keyword_lists.numbers_prefix1, "$ 0x");
        assert_eq!(udl.keyword_lists.numbers_extras1, "_");
        assert_eq!(udl.keyword_lists.numbers_suffix1, "L UL");
        assert_eq!(udl.keyword_lists.numbers_extras2, ".");
    }

    #[test]
    fn cisco_ios_shaped_v2_0_udl_loads_end_to_end() {
        // Regression pin against Luis Pisco's Cisco IOS UDL
        // shape (v2.0, empty `ext`, non-empty Keywords1..6, no
        // `<Prefix>` numeric attrs missing, v2.0 keyword-list
        // names for the numbers, populated `<Styles>` set). Not
        // the actual bundled file (third-party asset we don't
        // redistribute), but a minimal reduction that exercises
        // every attribute the v2.0 → v2.1 tolerance work
        // touches: `<Global>` without `@decimalSeparator`,
        // `<Keywords name="Numbers, additional">`, empty `ext`
        // attribute, and all 24 `<WordsStyle>` slots.
        let xml = r#"<NotepadPlus>
            <UserLang name="Cisco IOS" ext="" udlVersion="2.0">
              <Settings>
                <Global caseIgnored="yes" allowFoldOfComments="no"
                        forcePureLC="0" foldCompact="no" />
                <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                        Keywords4="no" Keywords5="yes" Keywords6="no"
                        Keywords7="no" Keywords8="no" />
              </Settings>
              <KeywordLists>
                <Keywords name="Comments">00! 00remark 01 02((EOF)) 03 04</Keywords>
                <Keywords name="Numbers, additional">:</Keywords>
                <Keywords name="Operators1">. /</Keywords>
                <Keywords name="Keywords1">interface hostname</Keywords>
                <Keywords name="Keywords5">FastEthernet Vlan</Keywords>
                <Keywords name="Delimiters">00 01 02 03 04 05 06 07 08 09 10 11 12 13 14 15 16 17 18 19 20 21 22 23</Keywords>
              </KeywordLists>
              <Styles>
                <WordsStyle name="DEFAULT" fgColor="000000" bgColor="FFFFFF" fontName="" fontStyle="0" nesting="0" />
                <WordsStyle name="KEYWORDS1" fgColor="AC8202" bgColor="FFFFFF" fontName="" fontStyle="1" nesting="0" />
                <WordsStyle name="KEYWORDS5" fgColor="0080FF" bgColor="FFFFFF" fontName="" fontStyle="0" nesting="0" />
              </Styles>
            </UserLang>
          </NotepadPlus>"#;
        let udl = UdlDefinition::parse(xml).expect("Cisco-IOS-shaped v2.0 UDL must parse");
        assert_eq!(udl.name, "Cisco IOS");
        assert!(
            udl.extensions.is_empty(),
            "empty @ext must yield an empty extension list, not a spurious [\"\"]"
        );
        // Prefix flag on Keywords5 preserved.
        assert!(udl.prefix[4], "Keywords5 prefix mode should be true");
        // v2.0 "Numbers, additional" aliased into extras2.
        assert_eq!(udl.keyword_lists.numbers_extras2, ":");
        // Ordinary keyword lists untouched.
        assert_eq!(udl.keyword_lists.keywords[0], "interface hostname");
        assert_eq!(udl.keyword_lists.keywords[4], "FastEthernet Vlan");
        // Style slots preserved with their v2.0 fontStyle bits.
        let kw1 = udl
            .styles
            .iter()
            .find(|s| s.name == "KEYWORDS1")
            .expect("KEYWORDS1 must load");
        assert_eq!(kw1.fg_color, 0x00AC_8202);
        assert_eq!(kw1.font_style, 1, "bold bit preserved");
    }

    #[test]
    fn unknown_keyword_list_names_are_skipped_not_errors() {
        // Forward compatibility: a future N++ that adds a new
        // <Keywords name="Something New"> entry shouldn't break
        // Code++'s loading of that file's other entries.
        let xml = r#"<NotepadPlus>
            <UserLang name="Forward" ext="fw" udlVersion="2.2">
              <Settings>
                <Global caseIgnored="no" allowFoldOfComments="no"
                        foldCompact="no" forcePureLC="0" decimalSeparator="0" />
                <Prefix Keywords1="no" Keywords2="no" Keywords3="no"
                        Keywords4="no" Keywords5="no" Keywords6="no"
                        Keywords7="no" Keywords8="no" />
              </Settings>
              <KeywordLists>
                <Keywords name="Keywords1">alpha beta</Keywords>
                <Keywords name="Something Novel">will be skipped</Keywords>
              </KeywordLists>
              <Styles />
            </UserLang>
          </NotepadPlus>"#;
        let udl = UdlDefinition::parse(xml).expect("forward-compat parse must succeed");
        assert_eq!(udl.keyword_lists.keywords[0], "alpha beta");
    }
}
