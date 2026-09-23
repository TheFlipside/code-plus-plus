//! Which characters may reach UI chrome verbatim.
//!
//! The classifier lives here, below every UI crate and below
//! `codepp_shell`, so that text the host builds an identity from can be
//! refused at the point the identity is constructed. A dock panel's key
//! read back from `session.xml` is the case that needs it: registration
//! sanitizes a plugin's panel name before interning it, but a restore
//! reads the key raw, and the interned name is what every caption and
//! tab label draws — see [`crate::dock::intern_plugin_panel`]. The
//! substitution helpers the chrome calls (`codepp_shell`'s
//! `sanitize_str_for_display` and its siblings) wrap this one function,
//! so there is still a single list.

/// True for characters that must never reach UI chrome verbatim.
///
/// Filenames are attacker-influenced: a plugin can pick one via
/// `NPPM_DOOPEN`, and a user can be induced to open a file out of an
/// untrusted archive. Non-Windows filesystems reachable from Windows
/// (WSL, Samba, sync tools) happily store names that `CreateFileW` on
/// native storage would refuse. Four classes cause real harm:
///
///   - **C0 controls and DEL, and the C1 range.** An embedded U+0000
///     silently truncates `SetWindowTextW` / `SB_SETTEXTW` on Win32 and
///     `gtk_window_set_title` on GTK, so the chrome names a *different*
///     file than the one that is open — which invites the user to save,
///     delete, or run the wrong one. TAB forges Win32's
///     accelerator-hint column. U+0085 NEL is a mandatory line break to
///     Uniscribe/DirectWrite and Pango alike.
///   - **Line and paragraph separators** (U+2028/U+2029). Same
///     mandatory-break treatment per UAX #14: they split a single-line
///     label across several visual lines and corrupt a segmented status
///     bar's layout.
///   - **Bidi marks, embeddings, overrides and isolates.** U+202E and
///     friends flip visible order so a label stops matching its path —
///     the classic `photo_gnp.exe` → `photo_exe.png` spoof (CWE-451).
///   - **Invisible zero-width characters** (ZWSP, word joiner, BOM). A
///     decoy `notes.txt␣ZWSP␣` renders pixel-identical to a genuine
///     `notes.txt` tab while being a different file.
///
/// **U+200C ZWNJ and U+200D ZWJ are deliberately *not* listed, and that
/// is a real residual risk, not a free win.** Both do genuine
/// orthographic work — ZWNJ in Persian and Indic scripts, ZWJ in every
/// multi-person emoji sequence — so neutralising them visibly corrupts
/// legitimate filenames. But the cost of keeping them is honest: a bare
/// ZWJ between two Latin letters, where no ligature rule applies, has
/// no visible effect in most fonts, which makes `report␣ZWJ␣.txt`
/// indistinguishable on screen from `report.txt` — exactly the collision
/// U+200B is filtered to prevent. The trade is "certain corruption of
/// real names" against "a narrower version of a spoof we otherwise
/// block", and it went the way it did because the first harm is
/// unconditional. Closing it properly means context-aware handling
/// (preserve only when adjacent to a joining or combining script),
/// which is worth doing if this class ever shows up in practice.
///
/// The invisible-character list is also a denylist, so it trails
/// Unicode by construction: `Cf`-category codepoints such as the Tag
/// block (U+E0000–U+E007F) reproduce the same primitive. Keying off the
/// `Cf` general category with ZWNJ/ZWJ as named carve-outs would be
/// self-updating, at the cost of a Unicode-table dependency.
#[must_use]
pub fn is_display_hostile(c: char) -> bool {
    matches!(c,
        // C0 controls (NUL, TAB, LF, CR, …), DEL, and C1 (incl. NEL).
        '\u{0000}'..='\u{001F}' | '\u{007F}'..='\u{009F}'
        // Zero-width space, word joiner, and the BOM as ZWNBSP.
        | '\u{200B}' | '\u{2060}' | '\u{FEFF}'
        // Bidi. This is the complete `Bidi_Control=Yes` set minus
        // nothing: ALM, the two directional marks, the embeddings and
        // overrides, and the isolates — twelve codepoints. ALM is the
        // weakest of them (it steers neutrals locally rather than
        // reversing a span) but it is invisible and in the class, so
        // leaving it out would make the set arbitrary.
        | '\u{061C}'
        | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        // Line and paragraph separators.
        | '\u{2028}' | '\u{2029}'
    )
}

#[cfg(test)]
mod tests {
    use super::is_display_hostile;

    /// One representative of each class the policy rejects, and the
    /// characters it deliberately lets through.
    #[test]
    fn the_classifier_rejects_each_hostile_class_and_nothing_else() {
        for hostile in [
            '\u{0}', '\t', '\n', '\u{7F}', '\u{85}', // C0, DEL, C1 (NEL)
            '\u{200B}', '\u{2060}', '\u{FEFF}', // zero-width
            '\u{061C}', '\u{200E}', '\u{202E}', '\u{2066}', '\u{2069}', // bidi
            '\u{2028}', '\u{2029}', // line / paragraph separators
        ] {
            assert!(is_display_hostile(hostile), "{hostile:?} must be rejected");
        }
        for fine in [
            'a',
            ' ',
            'é',
            '\u{200C}',
            '\u{200D}',
            '\u{FFFD}',
            '\u{1F600}',
        ] {
            assert!(!is_display_hostile(fine), "{fine:?} must pass through");
        }
    }
}
