//! Headless core for Code++.
//!
//! This crate intentionally has no UI, no Scintilla, and no platform
//! code. It is unit-testable without an OS event loop. See DESIGN.md
//! §2.2 and §5.1–§5.2.

pub mod encoding;
pub mod eol;
pub mod fif;
pub mod file;
pub mod find_history;
pub mod lang;
pub mod npp_session;
pub mod preferences;
pub mod recent_files;
pub mod session;
pub mod styles;

pub use encoding::{Encoding, EncodingError};
pub use eol::Eol;
pub use fif::{
    FifMatch, FifQuery, FifQueryError, FifQueryOpts, FifWalkOpts, FifWalkOptsError,
    FileSearchOutcome,
};
pub use file::{
    LoadError, LoadErrorKind, LoadResult, LoadedFile, Loader, LoaderShutdown, RequestId,
};
pub use find_history::{FindHistory, FindHistoryError};
pub use lang::LangType;
pub use preferences::{
    Preferences, PreferencesError, RecentFileDisplayMode, RecentFilesHistoryConfig,
};
pub use recent_files::{RecentFiles, RecentFilesError};
pub use session::{Session, SessionError, Tab, WindowGeometry};
pub use styles::{format_rgb_hex, parse_rgb_hex, StyleEntry, Styles, StylesError, Transparency};

/// A workspace-wide source lint, not a unit test.
///
/// It lives in `core` because `core` is the crate every other one
/// depends on, so the guard runs whenever anything is tested; and it
/// lives at the crate root rather than inside a feature module because
/// what it checks — that no `tracing` call site formats with `Display`
/// — is a property of the whole workspace, not of any one subsystem.
#[cfg(test)]
mod tracing_sigil_guard {
    /// True if `line` contains a Display-sigil tracing field.
    ///
    /// Two shapes, and missing the second is what made an earlier
    /// version of this guard pass vacuously while five real offenders
    /// sat inside its own scan root:
    ///
    ///   * **named** — an identifier, ` = `, a percent sign, an
    ///     identifier;
    ///   * **shorthand** — a percent sign directly after `(` or `, `
    ///     inside the macro call, which `tracing` expands to the named
    ///     form using the expression as its own field name.
    ///
    /// Spelled out in prose rather than shown literally, because the
    /// guard scans this file too. Deliberately crude: it does not parse
    /// Rust, and it deliberately does not try to prove the line is
    /// inside a `tracing` macro — a false positive is a five-second
    /// fix, a false negative is a live vulnerability.
    ///
    /// It is blind to the non-macro recording API
    /// (`tracing::field::display(...)`), which no code in the tree uses
    /// today. "No Display sigil" is therefore not the same claim as
    /// "nothing is ever Display-formatted into a log"; if that API
    /// appears, this needs extending.
    fn has_display_sigil(line: &str) -> bool {
        let b = line.as_bytes();
        let ident_start = |c: u8| c.is_ascii_alphabetic() || c == b'_';
        // What can begin the expression after a sigil. The two
        // positions differ, and treating them alike is what let an
        // earlier version miss real calls: `tracing`'s grammar
        // restricts the *shorthand* form to an identifier path, but
        // the *named* form takes an arbitrary expression — so a
        // dereference, a parenthesised sub-expression, a negation and
        // a borrow are all valid there and all compile. (Spelled in
        // prose: this guard scans its own source, so literal examples
        // here would flag this file.)
        //
        // The set stays deliberately small rather than "any byte":
        // `crates/editor/src/theme.rs` carries doc comments describing
        // JSP's expression-tag syntax, whose characters match the
        // named shape exactly. Accepting anything after the sigil
        // flags those two lines.
        let expr_start = |c: u8| ident_start(c) || matches!(c, b'(' | b'*' | b'&' | b'!' | b'-');
        for (i, &c) in b.iter().enumerate() {
            if c != b'%' {
                continue;
            }
            // Skip horizontal whitespace on both sides before deciding
            // anything. Spacing inside a macro's token tree is free —
            // and, critically, **`cargo fmt` will not normalise it**:
            // a sigil expression is not parseable as a Rust expression
            // (there is no unary `%` or `?` operator), so rustfmt
            // cannot format the argument list and leaves the tokens
            // byte-for-byte. Verified against the pinned rustfmt.
            //
            // An earlier version compared the immediately-adjacent
            // bytes, which meant one missing space around the `=` —
            // an ordinary typo — produced real, compiling,
            // Display-formatting code that neither this guard nor
            // `cargo fmt --check` would say a word about. Seven of
            // eight realistic spacing variants slipped through.
            let mut lo = i;
            while lo > 0 && matches!(b[lo - 1], b' ' | b'\t') {
                lo -= 1;
            }
            let mut hi = i + 1;
            while hi < b.len() && matches!(b[hi], b' ' | b'\t') {
                hi += 1;
            }
            let next = b.get(hi).copied().unwrap_or(b' ');
            // `None` means only whitespace precedes it on this line,
            // which is where rustfmt puts a wrapped field.
            let prev = (lo > 0).then(|| b[lo - 1]);
            let named = prev == Some(b'=') && expr_start(next);
            let shorthand =
                ident_start(next) && (prev == Some(b'(') || prev == Some(b',') || prev.is_none());
            if named || shorthand {
                return true;
            }
        }
        false
    }

    /// Every `.rs` file in the workspace, vendored code excluded.
    fn workspace_rs_files() -> Vec<std::path::PathBuf> {
        // `crates/core` -> `crates` -> workspace root, so `plugins/`
        // and `tools/` are covered too. They have no `tracing` calls
        // today, but the assertion message claims the whole workspace
        // and must not be narrower than it says.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("crates/core -> crates -> workspace root")
            .to_path_buf();
        let mut out = Vec::new();
        collect_rs(&root, &mut out);
        out
    }

    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // `vendor/` is Scintilla and Lexilla, `target/` is
                // build output, `.git/` is not source. None are ours.
                if matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("vendor" | "target" | ".git")
                ) {
                    continue;
                }
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn the_display_sigil_detector_catches_both_shapes() {
        // This test exists because the guard below once passed while
        // five real offenders sat in the tree: the detector only knew
        // the named shape and was never checked against a known
        // positive. A guard that has not been shown to fail is not a
        // guard.
        // The fixtures compose the sigil at run time rather than
        // spelling it literally, because the workspace guard below
        // scans this file too and a literal example would make it
        // report its own test data. The tempting alternative — teach
        // the walk to skip this file — would have to skip it by name,
        // the same crude way `collect_rs` already skips directories,
        // and `lib.rs` is every crate's root module: it would blind
        // the guard to `crates/shell/src/lib.rs`, which is full of
        // real and important `tracing::warn!` calls.
        let sig = '%';
        let positives = [
            format!(r#"tracing::warn!(error = {sig}e, "boom");"#),
            format!(r#"tracing::warn!({sig}err, "save failed");"#),
            format!(r#"tracing::warn!({sig}err, dirty, "decode failed");"#),
            format!(r#"tracing::info!(count, path = {sig}p, "x");"#),
            format!(r#"    tracing::error!({sig}err, "gtk::init failed");"#),
            // Alone on a continuation line — what rustfmt produces for
            // a multi-field call with a long message, and what an
            // earlier detector missed. Proven by planting exactly this
            // at `ui_gtk/src/tabs.rs` and watching the guard pass.
            format!("                {sig}err,"),
            format!("    {sig}path,"),
            // Named position takes an arbitrary expression, not just
            // an identifier — these all compile as `tracing` calls and
            // were all missed by an earlier detector.
            format!(r#"tracing::warn!(guard = {sig}*guard, "x");"#),
            format!(r#"tracing::warn!(delta = {sig}(a - b), "x");"#),
            format!(r#"tracing::warn!(off = {sig}-offset, "x");"#),
            format!(r#"tracing::warn!(r = {sig}&x, "x");"#),
            // Spacing variants. `cargo fmt` does not normalise any of
            // these — a sigil expression will not parse as a Rust
            // expression, so rustfmt leaves the token spacing alone —
            // which makes a single missing space a silent hole rather
            // than something the formatter tidies away.
            format!(r#"tracing::warn!(error={sig}e, "boom");"#),
            format!(r#"tracing::warn!(error ={sig}e, "boom");"#),
            format!(r#"tracing::warn!(error  =  {sig}e, "boom");"#),
            format!(r#"tracing::warn!({sig}  e, "boom");"#),
            format!(r#"tracing::warn!(  {sig}e, "boom");"#),
            format!(r#"tracing::warn!(a,{sig}e, "boom");"#),
            format!(r#"tracing::warn!(a  ,  {sig}e, "boom");"#),
        ];
        for positive in &positives {
            assert!(
                has_display_sigil(positive),
                "missed a Display sigil in: {positive}"
            );
        }
        let negatives = [
            r#"tracing::warn!(error = ?e, "boom");"#.to_string(),
            r#"tracing::warn!(?err, "save failed");"#.to_string(),
            "let x = n % 4;".to_string(),
            // NASM/SAS keyword lists in `lang.rs` look like sigils to a
            // crude scanner; they are string data, and must not trip it.
            format!(r#""{sig}macro {sig}endmacro {sig}imacro""#),
            format!("// 50{sig} of the budget"),
            format!(r#"format!("{{n}}{sig}")"#),
            // JSP tag syntax, described in `editor/src/theme.rs`'s doc
            // comments. Contains the named-position characters but is
            // prose, and a detector that accepts any byte after the
            // sigil flags it.
            format!("/// directive, `<{sig} {sig}>` scriptlet, `<{sig}= {sig}>` expression,"),
            format!("///   - `<{sig} {sig}>` / `<{sig}= {sig}>` / `<{sig}! {sig}>` all enter the"),
        ];
        for negative in &negatives {
            assert!(
                !has_display_sigil(negative),
                "false positive on: {negative}"
            );
        }
    }

    #[test]
    fn no_tracing_call_site_uses_the_display_sigil() {
        // `tracing`'s Display sigil formats with `Display`, which
        // passes control bytes and ANSI escape sequences through
        // untouched; the Debug sigil escapes them. With a subscriber
        // now installed, a Display-formatted filename, plugin name or
        // `session.xml` attribute — all attacker-influenced per this
        // project's threat model — reaches the developer's terminal
        // raw, where an xterm OSC sequence can retitle the window and
        // a CR can overwrite the line above it. Verified by planting
        // an OSC escape in a `session.xml` `eol` attribute and
        // watching it arrive unescaped on stderr.
        //
        // Nothing in `clippy::pedantic` catches this and the sigil is
        // one character, so this guard is the only thing between a
        // future edit and a silent regression.
        let files = workspace_rs_files();
        assert!(
            files.len() > 50,
            "scanned only {} files; the walk is broken, so a clean \
         result would prove nothing",
            files.len()
        );
        let mut offenders = Vec::new();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (n, line) in text.lines().enumerate() {
                if has_display_sigil(line) {
                    offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "tracing call sites using the Display sigil — switch them to the Debug sigil \
         so control characters are escaped:\n{}",
            offenders.join("\n")
        );
    }
}
