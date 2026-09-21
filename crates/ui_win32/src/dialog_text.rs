//! Body text for a `MessageBoxW`, sanitized at construction.
//!
//! [`DialogText`] exists so that [`crate::show_error_dialog`] cannot be
//! handed raw text. Its field is private and this module exposes no
//! constructor that skips [`sanitize_str_for_display`], so a caller
//! composing an error message from a `ShellError`'s `Display`, an
//! `io::Error`, or a path has exactly one way to get it into the
//! dialog — and that way sanitizes. The parameter type is the guard.
//!
//! **Why the dialog function does not simply sanitize its argument.**
//! Several callers build a body with deliberate structure — a
//! sentence, a blank line, the path, a blank line, the OS error — and
//! the sanitizer replaces `\n` with U+FFFD, because an embedded newline
//! in untrusted text injects a line the surrounding prose makes look
//! official. A flat `&str` cannot tell a caller-authored line break
//! from one that arrived inside an error string, so a sanitize-inside
//! fix would have to either destroy the structure or preserve the
//! injection. Sanitizing *per part* before joining is the only shape
//! that keeps both properties, and that is what [`DialogText::lines`]
//! and [`DialogText::paragraphs`] do.
//!
//! **Why a type rather than call-site discipline.** `ui_gtk` and
//! `ui_cocoa` sanitize at each call site, and every current site there
//! does. This backend had the same rule and did not follow it: five
//! sites composed error text — a `ShellError`'s `Display`, or the
//! Recycle Bin's status string — into the message unsanitized, and
//! DESIGN.md §7.4 records this as the third instance of the same
//! omission after `NPPM_SETSTATUSBAR` and the workspace tree. A rule
//! that has to be remembered at every new site is not a guard; a
//! parameter type is.
//!
//! The two confirmation prompts in `lib.rs` — reload and Move to
//! Recycle Bin — each compose their body inside one tested helper from
//! a path that goes through `sanitize_path_for_display`, and take no
//! runtime text from their callers, so they are not routed through
//! this type. They could be; nothing here is specific to errors.

use codepp_shell::sanitize_str_for_display;

/// Message-box body text whose every character has passed the shared
/// display policy (`codepp_shell::sanitize_display_char`).
///
/// Only the three constructors can produce one. Pick by shape:
///
/// * [`DialogText::sanitized`] — one paragraph. Any `\n` in the input
///   is substituted, so a caller that wants line structure must use
///   one of the other two rather than embed `\n` in the string.
/// * [`DialogText::paragraphs`] — parts separated by a blank line.
/// * [`DialogText::lines`] — parts one per line.
///
/// The joins are inserted *after* each part is sanitized, which is
/// what makes them trustworthy: a part can carry whatever an error
/// string carries, and the only line breaks in the result are the
/// ones the caller's code put there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DialogText(String);

impl DialogText {
    /// A single paragraph, sanitized. Fine for a literal too — the
    /// policy is a no-op on plain text, and there is deliberately no
    /// `literal` constructor that would let a `&'static str` bypass
    /// the type's one guarantee.
    #[must_use]
    pub(crate) fn sanitized(text: &str) -> Self {
        Self(sanitize_str_for_display(text))
    }

    /// Parts joined by a blank line (`\n\n`), each sanitized first.
    #[must_use]
    pub(crate) fn paragraphs<S: AsRef<str>>(parts: impl IntoIterator<Item = S>) -> Self {
        Self::joined(parts, "\n\n")
    }

    /// Parts joined one per line (`\n`), each sanitized first.
    #[must_use]
    pub(crate) fn lines<S: AsRef<str>>(parts: impl IntoIterator<Item = S>) -> Self {
        Self::joined(parts, "\n")
    }

    /// The text to hand to `MessageBoxW`.
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn joined<S: AsRef<str>>(parts: impl IntoIterator<Item = S>, separator: &str) -> Self {
        let mut out = String::new();
        for (index, part) in parts.into_iter().enumerate() {
            if index > 0 {
                out.push_str(separator);
            }
            out.push_str(&sanitize_str_for_display(part.as_ref()));
        }
        Self(out)
    }
}

#[cfg(test)]
mod tests {
    //! Pure, so it runs on the Windows CI runner with no HWND. The
    //! dialog function itself cannot be driven from `cargo test`; what
    //! can be pinned is the one property it relies on — that nothing
    //! reaches it unsanitized and that caller-authored structure
    //! survives while embedded structure does not.
    use super::DialogText;

    #[test]
    fn sanitized_substitutes_the_hostile_characters_an_error_string_can_carry() {
        // The §7.4 scenario: a save failure naming a file whose name
        // carries a right-to-left override, which unsanitized would
        // render the extension reversed in the dialog.
        let err = "I/O error: C:\\invoice\u{202E}fdp.exe: access denied";
        assert_eq!(
            DialogText::sanitized(err).as_str(),
            "I/O error: C:\\invoice\u{FFFD}fdp.exe: access denied"
        );
        // Control characters and both line-break bytes are substituted
        // too: a `\n` inside untrusted text is exactly the fake-line
        // injection the dialog must not render.
        assert_eq!(
            DialogText::sanitized("a\tb\r\nc\u{200B}d").as_str(),
            "a\u{FFFD}b\u{FFFD}\u{FFFD}c\u{FFFD}d"
        );
    }

    #[test]
    fn sanitized_passes_plain_text_through_unchanged() {
        let plain = "No printer is installed, or the default printer could not be opened.";
        assert_eq!(DialogText::sanitized(plain).as_str(), plain);
        // Non-ASCII that is not in the hostile set is kept as-is.
        assert_eq!(
            DialogText::sanitized("Save All — 2 failed").as_str(),
            "Save All — 2 failed"
        );
    }

    #[test]
    fn paragraphs_keep_the_joins_and_sanitize_each_part() {
        // The property that a sanitize-inside-the-dialog fix could not
        // provide: the caller's blank lines survive, while a newline
        // *inside* a part — here, inside the error text — does not.
        let text = DialogText::paragraphs([
            "Could not write the session file:",
            "C:\\s\u{202E}noisses.xml",
            "I/O error: disk full\nPress OK to retry",
        ]);
        assert_eq!(
            text.as_str(),
            "Could not write the session file:\n\n\
             C:\\s\u{FFFD}noisses.xml\n\n\
             I/O error: disk full\u{FFFD}Press OK to retry"
        );
    }

    #[test]
    fn lines_join_with_a_single_newline_and_accept_owned_strings() {
        // The Save All summary builds one `String` per failed buffer.
        let parts: Vec<String> = vec!["buffer 3: I/O error".into(), "buffer 5: a\tb".into()];
        assert_eq!(
            DialogText::lines(parts).as_str(),
            "buffer 3: I/O error\nbuffer 5: a\u{FFFD}b"
        );
    }

    #[test]
    fn a_single_part_gets_no_separator_and_no_parts_give_empty_text() {
        assert_eq!(DialogText::paragraphs(["only"]).as_str(), "only");
        assert_eq!(DialogText::lines(["only"]).as_str(), "only");
        assert_eq!(DialogText::paragraphs(Vec::<&str>::new()).as_str(), "");
        assert_eq!(DialogText::lines(Vec::<&str>::new()).as_str(), "");
    }

    #[test]
    fn sanitizing_is_idempotent() {
        // Paths reach `paragraphs` already run through
        // `sanitize_path_for_display`, so a second pass over U+FFFD
        // must be a no-op or the two policies would visibly drift.
        let once = DialogText::sanitized("x\u{202E}y\u{0000}z");
        let twice = DialogText::sanitized(once.as_str());
        assert_eq!(once, twice);
    }
}
