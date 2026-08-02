//! UDL container-lexer wiring for the Cocoa backend.
//!
//! The tokeniser (`codepp_udl`) and the paint logic
//! (`codepp_editor::udl_paint`) are cross-platform; this module is the
//! Cocoa glue that connects them to the live editor. It is a close port
//! of `ui_gtk::udl`, which is itself a port of Win32's `apply_udl_lang` /
//! `handle_udl_style_needed` pair: [`apply_lang`] routes a language
//! switch to the container lexer, and [`on_style_needed`] drives the
//! `SCN_STYLENEEDED` notification.
//!
//! # Why a UDL needs any host code at all
//!
//! Every other language in `LANG_TABLE` is a Lexilla lexer: the host
//! sends `SCI_SETILEXER` and Scintilla does the tokenising itself.
//! Notepad++'s UDL format has no Lexilla lexer, so Code++ runs it in
//! `SCLEX_CONTAINER` mode — Scintilla asks the *host* to style a range,
//! by way of `SCN_STYLENEEDED`, and the host walks the UDL's rules. That
//! is the whole of the difference, and it is why a backend that never
//! answers `SCN_STYLENEEDED` renders a UDL buffer entirely unstyled
//! rather than wrongly styled.

use codepp_core::LangType;
use codepp_editor::EditorHandle;

use crate::state::with_state;

/// If `lang` is a UDL, put the editor into container-lexer mode with the
/// UDL's palette and return `true`; otherwise return `false` so the
/// caller falls through to the Lexilla theme path.
///
/// Called from `platform.rs`'s `apply_lang`, which runs during a `drain`
/// (the split `&mut Shell` borrow is live), so the registry is reached
/// through the read-only pointer captured in `CocoaUiState::split`
/// rather than `with_state` — a nested borrow there would be declined.
///
/// # Safety
///
/// `registry` must be the pointer `CocoaUiState::split` captured from
/// `Shell.udl_registry`. The read is sound under the aliasing discipline
/// documented on the `CocoaUi::udl_registry` field (shared read, never
/// concurrent with a `&mut UdlRegistry`).
pub(crate) fn apply_lang(
    editor: &EditorHandle,
    registry: *const codepp_udl::UdlRegistry,
    lang: LangType,
) -> bool {
    if !codepp_udl::is_udl_lang_id(lang.as_npp_id()) {
        return false;
    }
    // SAFETY: read-only deref of the registry pointer per the fn contract.
    let registry = unsafe { &*registry };
    let Some(entry) = registry.find_by_lang_type_id(lang.as_npp_id()) else {
        // A UDL id with no registry entry shouldn't happen — `Shell` only
        // hands out ids it registered — but degrade to plain text rather
        // than panic if it ever does.
        tracing::warn!(
            lang = lang.as_npp_id(),
            "UDL LangType not in registry; falling back to plain text"
        );
        return false;
    };
    // Clone out of the registry borrow (cheap: a handful of small structs
    // plus an Arc refcount bump) so the paint below holds no borrow of
    // the registry — and, defensively, no read alias of `Shell` across
    // the Scintilla calls.
    let styles = entry.definition.styles.clone();
    let compiled = std::sync::Arc::clone(&entry.compiled);
    codepp_editor::udl_paint::apply_udl_lang(editor, &styles, &compiled);
    true
}

/// The status-bar label for `lang`: a UDL's own name (from its
/// `<UserLang name="...">`, sanitized) for a UDL id, else the built-in
/// language name. Mirror of GTK's `resolve_lang_label` and Win32's.
/// Without it a UDL buffer reads as "Normal Text" in the status bar even
/// while the container lexer is styling it.
///
/// # Safety
///
/// Same contract as [`apply_lang`]: `registry` must be the read-only
/// pointer `CocoaUiState::split` captured from `Shell.udl_registry`.
pub(crate) fn resolve_lang_label(
    lang: LangType,
    registry: *const codepp_udl::UdlRegistry,
) -> String {
    if !codepp_udl::is_udl_lang_id(lang.as_npp_id()) {
        return lang.language_name().unwrap_or("Normal Text").to_string();
    }
    // SAFETY: read-only deref of the registry pointer per the fn contract.
    let registry = unsafe { &*registry };
    registry.find_by_lang_type_id(lang.as_npp_id()).map_or_else(
        || "Normal Text".to_string(),
        // A UDL-authored name is untrusted display text, same policy as
        // filenames — sanitize before it reaches the status-bar chrome.
        |entry| codepp_shell::sanitize_filename_for_display(&entry.definition.name),
    )
}

/// The loaded UDLs as `(lang id, sanitized name)`, for the Language
/// menu's flat rows. Empty when no UDL is installed, which is the
/// common case and is why the menu's separator is conditional.
///
/// Takes its own `with_state` borrow — unlike [`apply_lang`], this runs
/// from `menuNeedsUpdate:` with no borrow held, so the raw pointer is
/// unnecessary here. Returns `None` if that borrow is declined, which
/// the caller must not confuse with "no UDLs installed": rebuilding the
/// rows to an empty list on a re-entrant call would silently empty the
/// menu.
pub(crate) fn language_rows() -> Option<Vec<(i32, String)>> {
    with_state(|st| {
        st.shell
            .udl_registry
            .entries()
            .iter()
            .map(|e| {
                (
                    e.lang_type_id,
                    codepp_shell::sanitize_filename_for_display(&e.definition.name),
                )
            })
            .collect()
    })
}

/// Handle an `SCN_STYLENEEDED` for the active buffer if it is a UDL.
/// `target` is the notification's `position` — the byte offset up to
/// which Scintilla wants styling. Called from `on_sci_notify_inner`.
///
/// Captures the editor handle (`Copy`) and the compiled rules (`Arc`)
/// under a `with_state` borrow, then **drops the borrow before
/// painting**: every `SCI_SETSTYLING` fires a re-entrant `SCN_MODIFIED`
/// that re-enters `on_sci_notify`, and a still-held `with_state` borrow
/// would make those silently no-op — the exact contract `on_sci_notify`
/// documents and that this backend has already been caught by three
/// times. The Cocoa analogue of the GTK and Win32 borrow-drop.
pub(crate) fn on_style_needed(target: usize) {
    let captured = with_state(|st| {
        let tab = st.shell.active()?;
        let lang_id = tab.lang.as_npp_id();
        if !codepp_udl::is_udl_lang_id(lang_id) {
            return None;
        }
        let entry = st.shell.udl_registry.find_by_lang_type_id(lang_id)?;
        Some((st.editor, std::sync::Arc::clone(&entry.compiled)))
    });
    // Outer `None`: re-entrant or uninstalled state. Inner `None`: the
    // active buffer isn't a UDL, or its registry entry vanished. Either
    // way there is nothing to paint.
    let Some(Some((editor, compiled))) = captured else {
        return;
    };
    codepp_editor::udl_paint::paint_style_needed(&editor, &compiled, target);
}
