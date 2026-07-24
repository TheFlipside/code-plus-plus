//! UDL container-lexer wiring for the GTK backend.
//!
//! The tokeniser (`codepp_udl`) and the paint logic
//! (`codepp_editor::udl_paint`) are cross-platform; this module is the GTK
//! glue that connects them to the live editor. It mirrors the `ui_win32`
//! `apply_udl_lang` / `handle_udl_style_needed` pair: [`apply_lang`] routes
//! a language switch to the container lexer, and [`on_style_needed`] drives
//! the `SCN_STYLENEEDED` notification.

use codepp_core::LangType;
use codepp_editor::EditorHandle;

use crate::state::with_state;

/// If `lang` is a UDL, put the editor into container-lexer mode with the
/// UDL's palette and return `true`; otherwise return `false` so the caller
/// falls through to the Lexilla theme path.
///
/// Called from `platform.rs::apply_lang`, which runs during a `drain` (the
/// split `&mut Shell` borrow is live), so the registry is reached through
/// the read-only pointer captured in `GtkUiState::split` rather than
/// `with_state`.
///
/// # Safety
///
/// `registry` must be the pointer `GtkUiState::split` captured from
/// `Shell.udl_registry`. The read is sound under the aliasing discipline
/// documented on the `GtkUi::udl_registry` field (shared read, never
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
        // A UDL id with no registry entry shouldn't happen — Shell only
        // hands out ids it registered — but degrade to plain text rather
        // than panic if it ever does.
        tracing::warn!(
            lang = lang.as_npp_id(),
            "UDL LangType not in registry; falling back to plain text"
        );
        return false;
    };
    // Clone out of the registry borrow (cheap: a handful of small structs
    // plus an Arc refcount bump) so the paint below holds no borrow of the
    // registry — and, defensively, no read alias of `Shell` across the
    // Scintilla calls.
    let styles = entry.definition.styles.clone();
    let compiled = std::sync::Arc::clone(&entry.compiled);
    codepp_editor::udl_paint::apply_udl_lang(editor, &styles, &compiled);
    true
}

/// The status-bar / display label for `lang`: a UDL's own name (from its
/// `<UserLang name="...">`, sanitized) for a UDL id, else the built-in
/// language name. Mirror of Win32's `resolve_lang_label` — without it a
/// UDL buffer shows "Normal Text" in the status bar even though it is
/// styled by the UDL.
///
/// # Safety
///
/// Same contract as [`apply_lang`]: `registry` must be the read-only
/// pointer `GtkUiState::split` captured from `Shell.udl_registry`, read
/// under the aliasing discipline on the `GtkUi::udl_registry` field.
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
        // A plugin/UDL-authored name is untrusted display text, same policy
        // as filenames — sanitize before it reaches the status-bar chrome.
        |entry| codepp_shell::sanitize_filename_for_display(&entry.definition.name),
    )
}

/// Handle an `SCN_STYLENEEDED` for the active buffer if it is a UDL.
/// `target` is the notification's `position` — the byte offset up to which
/// Scintilla wants styling. Called from the dedicated `sci-notify` handler
/// in `lib.rs`.
///
/// Captures the editor handle (Copy) and the compiled rules (Arc) under a
/// `with_state` borrow, then **drops the borrow before painting**: every
/// `SCI_SETSTYLING` fires a reentrant `SCN_MODIFIED` that re-enters the
/// `sci-notify` handlers, and a still-held `with_state` borrow would make
/// them silently no-op. The GTK analogue of the Win32 borrow-drop.
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
