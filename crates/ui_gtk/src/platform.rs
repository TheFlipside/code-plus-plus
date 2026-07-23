//! `UiPlatform` for the GTK backend.
//!
//! 34 of the trait's 48 methods exist on Linux; the other 14 are
//! `#[cfg(target_os = "windows")]` on the trait itself because their
//! signatures name plugin-host types that are Windows-only until the
//! plugin host is ported.
//!
//! Most of what is here is a direct port of `ui_win32`'s
//! implementation, because most of it is not UI code at all — it is
//! Scintilla messages sent through `EditorHandle`, which behaves
//! identically on both backends once the §4.2 direct-call pair is
//! captured. Where the Win32 body touched a Win32 control, the GTK
//! equivalent is used instead; where it touched nothing, the body is
//! the same sequence of messages in the same order, deliberately, so
//! the two backends cannot drift.

use codepp_core::styles::{parse_rgb_hex, Styles};
use codepp_core::{Encoding, Eol, LangType};
use codepp_editor::EditorHandle;
use codepp_scintilla_sys::{
    SCI_ADDUNDOACTION, SCI_BEGINUNDOACTION, SCI_COLOURISE, SCI_EMPTYUNDOBUFFER, SCI_ENDUNDOACTION,
    SCI_GETANCHOR, SCI_GETCOLUMN, SCI_GETCURRENTPOS, SCI_GETDOCPOINTER, SCI_GETFIRSTVISIBLELINE,
    SCI_GETLENGTH, SCI_GETLINECOUNT, SCI_GETMODIFY, SCI_GETOVERTYPE, SCI_GETSELECTIONEND,
    SCI_GETSELECTIONSTART, SCI_GETTEXT, SCI_GETXOFFSET, SCI_GETZOOM, SCI_GOTOPOS,
    SCI_LINEFROMPOSITION, SCI_LINESCROLL, SCI_LINESONSCREEN, SCI_POSITIONAFTER, SCI_SETDOCPOINTER,
    SCI_SETEMPTYSELECTION, SCI_SETEOLMODE, SCI_SETSAVEPOINT, SCI_SETSEL, SCI_SETSELECTIONEND,
    SCI_SETSELECTIONSTART, SCI_SETTABWIDTH, SCI_SETTEXT, SCI_SETXOFFSET, SCI_STYLEGETBACK,
    SCI_STYLEGETFORE, SC_DOCUMENTOPTION_DEFAULT, SC_EOL_CR, SC_EOL_CRLF, SC_EOL_LF, STYLE_DEFAULT,
};

/// Visible width of a TAB, in spaces. Matches `ui_win32`'s tab-width
/// default: 4 is the convention every wired lexer's style guide
/// prescribes, against Scintilla's built-in 8.
const TAB_WIDTH_SPACES: usize = 4;
/// Margin index for line numbers; margin 0 by Scintilla convention.
const LINE_NUMBER_MARGIN: u32 = 0;
/// Margin index for the change-history "edit indicator" strip. 4 sits to
/// the right of the line-number margin (and a future fold margin), the
/// same slot `ui_win32` uses so both backends match.
const CHANGE_HISTORY_MARGIN: u32 = 4;
/// Pixel width of the change-history strip when populated — a thin slice,
/// matching `ui_win32`.
const CHANGE_HISTORY_MARGIN_PX: i32 = 4;
/// Change-history strip colour: Material orange 400 in Scintilla's
/// `0x00BBGGRR` order, the same shade the active-tab indicator uses.
const CHANGE_HISTORY_COLOR: u32 = 0x00_26_A7_FF;

use codepp_shell::{SearchFlags, UiPlatform};
use gtk::prelude::*;

use crate::state::GtkUi;

/// Pack `(r, g, b)` into Scintilla's colour encoding, `0x00BBGGRR`.
///
/// Byte-identical to a Win32 `COLORREF`, which is why `ui_win32` can
/// reuse its own packer for both — but this is the *Scintilla* encoding
/// (documented in `Scintilla.h`), reachable on every backend, so the
/// GTK side names it for what it is rather than borrowing a Win32 type
/// name it has no business knowing.
const fn rgb_to_scintilla_colour((r, g, b): (u8, u8, u8)) -> u32 {
    (b as u32) << 16 | (g as u32) << 8 | (r as u32)
}

/// Caret/scroll position of the bound document, so a temporary
/// `SCI_SETDOCPOINTER` swap can put the user's view back exactly as it
/// was. Mirrors `ui_win32::ScintillaViewState`.
struct ViewState {
    caret: isize,
    anchor: isize,
    top_line: isize,
    x_offset: isize,
}

impl GtkUi {
    fn snapshot_view(&self) -> ViewState {
        ViewState {
            caret: self.editor.send(SCI_GETCURRENTPOS, 0, 0),
            anchor: self.editor.send(SCI_GETANCHOR, 0, 0),
            top_line: self.editor.send(SCI_GETFIRSTVISIBLELINE, 0, 0),
            x_offset: self.editor.send(SCI_GETXOFFSET, 0, 0),
        }
    }

    fn restore_view(&self, snap: &ViewState) {
        self.editor
            .send(SCI_SETSEL, snap.anchor.max(0) as usize, snap.caret);
        let cur_top = self.editor.send(SCI_GETFIRSTVISIBLELINE, 0, 0);
        let delta = snap.top_line - cur_top;
        if delta != 0 {
            self.editor.send(SCI_LINESCROLL, 0, delta);
        }
        self.editor
            .send(SCI_SETXOFFSET, snap.x_offset.max(0) as usize, 0);
    }

    /// Run `f` with `doc` temporarily bound to the view, restoring the
    /// previous document and the user's scroll/caret afterwards.
    ///
    /// The doc-pointer swap is how a single Scintilla view serves many
    /// tabs (DESIGN.md §7.2 Phase 3). Reading another tab's text is
    /// therefore visible to the *view*, not just the model, which is
    /// why the caret and scroll offsets have to be saved and put back —
    /// otherwise a Save All would leave the user staring at a different
    /// line than before.
    fn with_doc<R>(&mut self, doc: isize, f: impl FnOnce(&mut Self) -> R, absent: R) -> R {
        if doc == 0 {
            return absent;
        }
        let prior = self.editor.send(SCI_GETDOCPOINTER, 0, 0);
        if prior == doc {
            return f(self);
        }
        let view = self.snapshot_view();
        self.editor.send(SCI_SETDOCPOINTER, 0, doc);
        let out = f(self);
        if prior != 0 {
            self.editor.send(SCI_SETDOCPOINTER, 0, prior);
            self.restore_view(&view);
        }
        out
    }

    /// Scroll the caret into view only if it has gone off-screen, so a
    /// find that lands on an already-visible match does not jolt the
    /// viewport. Port of `ui_win32::center_caret_if_offscreen`.
    fn center_caret_if_offscreen(&self) {
        let pos = self.editor.send(SCI_GETCURRENTPOS, 0, 0).max(0) as usize;
        let line = self.editor.send(SCI_LINEFROMPOSITION, pos, 0).max(0);
        let first = self.editor.send(SCI_GETFIRSTVISIBLELINE, 0, 0).max(0);
        let lines = self.editor.send(SCI_LINESONSCREEN, 0, 0).max(1);
        if line >= first && line < first + lines {
            return;
        }
        let target = (line - lines / 2).max(0);
        self.editor.send(SCI_LINESCROLL, 0, target - first);
    }

    /// Refresh the status bar's caret/length parts from live editor
    /// state. Called by `update_status` and by the editor's own
    /// notification handler.
    pub fn refresh_dynamic_status(&self) {
        let length = self.editor.send(SCI_GETLENGTH, 0, 0).max(0) as u64;
        let lines = self.editor.send(SCI_GETLINECOUNT, 0, 0).max(0) as u64;
        let pos = self.editor.send(SCI_GETCURRENTPOS, 0, 0).max(0) as u64;
        let caret_line = self
            .editor
            .send(SCI_LINEFROMPOSITION, pos as usize, 0)
            .max(0) as u64;
        let caret_col = self.editor.send(SCI_GETCOLUMN, pos as usize, 0).max(0) as u64;
        let overtype = self.editor.send(SCI_GETOVERTYPE, 0, 0) != 0;
        self.status
            .set_dynamic_parts(length, lines, caret_line, caret_col, pos, overtype);
        // The line-number margin is a fixed width (sized once in
        // `enable_line_number_margin`), so nothing to re-measure here as the
        // line count changes — the gutter stays constant, matching Win32.
    }
}

/// Configure the predefined 32-39 styles that `SCI_STYLECLEARALL`
/// resets, then fix up the line-number margin for this backend.
///
/// The shared helper sets margin 0 to `SC_MARGIN_TEXT`, because
/// `ui_win32` renders the digits itself to get them right-aligned —
/// which means the host must write per-line margin text and keep it in
/// step with every edit. That machinery is Win32-private and not ported
/// yet, so a GTK buffer using `SC_MARGIN_TEXT` would show an empty
/// gutter. Override to Scintilla's built-in `SC_MARGIN_NUMBER`, which
/// formats and paints the numbers with no host involvement. The
/// difference is alignment only, and it is visible line numbers versus
/// none.
fn apply_predefined_styles(editor: &EditorHandle) {
    // `apply_line_number_margin` styles STYLE_LINENUMBER (fore/back) and,
    // for Win32's manual renderer, sets margin 0 to `SC_MARGIN_TEXT`.
    // GTK/Cocoa use Scintilla's built-in number margin, so override the
    // type and take a fixed, constant width via the shared method (the
    // gutter never grows while editing, matching Win32).
    codepp_editor::theme::apply_line_number_margin(editor);
    editor.enable_line_number_margin(LINE_NUMBER_MARGIN);
    // The change-history "edit indicator" strip. Shared config so it looks
    // and behaves identically to Win32 (and the coming Cocoa backend);
    // per-document enablement happens in `activate_tab`.
    editor.configure_change_history_margin(
        CHANGE_HISTORY_MARGIN,
        CHANGE_HISTORY_MARGIN_PX,
        CHANGE_HISTORY_COLOR,
    );
    codepp_editor::theme::apply_brace_styles(editor);
    codepp_editor::theme::apply_indent_guide_style(editor);
}

/// Read the whole document out of `editor` as a `String`.
///
/// Free function rather than a method so `capture_text_from_doc` can
/// reuse it inside `with_doc` without re-borrowing `self`.
fn read_all(editor: &EditorHandle) -> String {
    let len = editor.send(SCI_GETLENGTH, 0, 0);
    if len <= 0 {
        return String::new();
    }
    let cap = len as usize + 1;
    let mut buf = vec![0u8; cap];
    let written = editor.send(SCI_GETTEXT, cap, buf.as_mut_ptr() as isize);
    if written <= 0 {
        return String::new();
    }
    buf.truncate(written as usize);
    // Scintilla stores bytes, not validated UTF-8: a file that failed
    // to decode cleanly can leave invalid sequences in the buffer.
    // Lossy conversion keeps the editor usable instead of panicking.
    String::from_utf8_lossy(&buf).into_owned()
}

impl UiPlatform for GtkUi {
    fn activate_tab(&mut self, _idx: usize, scintilla_doc: isize) -> isize {
        // 0 means "this tab has no document yet" — mint one. Every
        // other value is a live doc pointer from a previous call.
        let fresh = scintilla_doc == 0;
        let doc = if fresh {
            self.editor.send(
                codepp_scintilla_sys::SCI_CREATEDOCUMENT,
                0,
                SC_DOCUMENTOPTION_DEFAULT,
            )
        } else {
            scintilla_doc
        };
        // Skip the swap when this doc is already bound. `SCI_SETDOCPOINTER`
        // clears the caret to 0 on every bind — even a redundant re-point
        // at the current document — so avoiding the no-op swap preserves
        // the caret whenever the view is already showing the target doc
        // (e.g. the trailing `bind_active_view` after a single-tab restore,
        // or re-activating the current tab). It does not help when the
        // active tab genuinely differs from what is bound; `restore_session`
        // re-seeds the caret explicitly for that case. Same shape as
        // `with_doc`'s `prior == doc` short-circuit below. A fresh doc is
        // never the current one, so it always binds.
        if doc != self.editor.send(SCI_GETDOCPOINTER, 0, 0) {
            self.editor.send(SCI_SETDOCPOINTER, 0, doc);
        }
        if fresh {
            // Tab width is *per-document* state in Scintilla, so it has
            // to be set on each new document rather than once at
            // startup. Without this a GTK buffer would render tabs at
            // Scintilla's built-in 8 columns while the Win32 build uses
            // 4 — the same file looking different on the two backends.
            self.editor.send(SCI_SETTABWIDTH, TAB_WIDTH_SPACES, 0);
            // Change-history tracking is per-document too: every fresh
            // SCI_CREATEDOCUMENT starts with it off. The margin itself is
            // view-level (configured once in apply_predefined_styles).
            self.editor.enable_change_history();
        }
        doc
    }

    fn set_buffer_text(&mut self, text: &str, cursor: u64) {
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        self.editor.send(SCI_SETTEXT, 0, bytes.as_ptr() as isize);
        // A freshly loaded file is not an edit: drop the undo history
        // that the SETTEXT itself created and mark the buffer clean, or
        // the user could Ctrl+Z their file back to empty.
        self.editor.send(SCI_EMPTYUNDOBUFFER, 0, 0);
        self.editor.send(SCI_SETSAVEPOINT, 0, 0);
        self.editor.send(SCI_GOTOPOS, cursor as usize, 0);
    }

    fn get_buffer_text(&mut self) -> String {
        read_all(&self.editor)
    }

    fn get_cursor_pos(&mut self) -> u64 {
        self.editor.send(SCI_GETCURRENTPOS, 0, 0).max(0) as u64
    }

    fn update_status(&mut self, lang: LangType, encoding: &Encoding, eol: Eol, _byte_len: u64) {
        // Keep Scintilla's own EOL mode in step, so newly typed lines
        // use the same ending as the rest of the file.
        let mode = match eol {
            Eol::CrLf => SC_EOL_CRLF,
            Eol::Cr => SC_EOL_CR,
            // `Mixed` has no Scintilla equivalent; LF is the least
            // surprising choice for new lines and matches Win32.
            Eol::Lf | Eol::Mixed => SC_EOL_LF,
        };
        self.editor.send(SCI_SETEOLMODE, mode, 0);
        // `language_name` is the same string the Language menu shows.
        // UDL ids are not in LANG_TABLE, so they fall back rather than
        // showing a blank part.
        let lang_label = lang.language_name().unwrap_or("Normal Text");
        self.status
            .set_static_parts(lang_label, eol.long_label(), encoding.label());
        self.refresh_dynamic_status();
    }

    fn set_plugin_status(&mut self, section: usize, text: &str) {
        self.status.set_plugin_part(section, text);
    }

    fn mark_saved(&mut self) {
        self.editor.send(SCI_SETSAVEPOINT, 0, 0);
    }

    fn apply_lang(&mut self, lang: LangType) {
        // UDL buffers need the container-lexer path, which is Phase 4.6
        // work not yet ported to GTK. Falling through to the Lexilla
        // path would land them in its plain-text fallback, which is the
        // correct degradation — but say so, rather than looking like
        // the theme table is broken.
        if codepp_udl::is_udl_lang_id(lang.as_npp_id()) {
            tracing::warn!(
                lang = lang.as_npp_id(),
                "UDL highlighting is not wired on GTK yet; rendering as plain text"
            );
        }
        codepp_editor::theme::apply_lang_theme(&self.editor, lang);
        // `apply_lang_theme` routes through `apply_default_styles`, which
        // resets margin 0 to `SC_MARGIN_TEXT` for Win32's manual renderer
        // (and re-styles STYLE_LINENUMBER). Re-assert the built-in number
        // margin here, or a file load / language change would blank the
        // gutter on GTK.
        self.editor.enable_line_number_margin(LINE_NUMBER_MARGIN);
    }

    fn apply_default_style(&mut self, styles: &Styles) {
        let entry = styles.effective_default();
        // Same fallbacks as the Win32 backend: black on white if the
        // user's styles.xml carries an unparseable colour, rather than
        // refusing to style at all.
        let fg = rgb_to_scintilla_colour(parse_rgb_hex(&entry.fg).unwrap_or((0, 0, 0)));
        let bg = rgb_to_scintilla_colour(parse_rgb_hex(&entry.bg).unwrap_or((0xFF, 0xFF, 0xFF)));

        self.editor.style_set_font(STYLE_DEFAULT, &entry.font_name);
        self.editor
            .style_set_size(STYLE_DEFAULT, i32::from(entry.font_size));
        self.editor.style_set_fore(STYLE_DEFAULT, fg);
        self.editor.style_set_back(STYLE_DEFAULT, bg);
        self.editor.style_set_bold(STYLE_DEFAULT, entry.bold);
        self.editor.style_set_italic(STYLE_DEFAULT, entry.italic);
        self.editor
            .style_set_underline(STYLE_DEFAULT, entry.underline);

        // Propagate to every other index, then put back the predefined
        // 32-39 styles that `SCI_STYLECLEARALL` just reset.
        self.editor.style_clear_all();
        apply_predefined_styles(&self.editor);

        // Win32 applies window transparency via WS_EX_LAYERED; the GTK
        // equivalent is the toplevel's opacity, which the compositor
        // honours when one is running and ignores otherwise.
        let transparency = styles.effective_transparency();
        self.window.set_opacity(if transparency.enabled {
            f64::from(transparency.percent.clamp(0, 100)) / 100.0
        } else {
            1.0
        });

        self.editor.send(SCI_COLOURISE, 0, -1);
    }

    fn search_next(&mut self, query: &str, flags: SearchFlags) -> Option<u64> {
        let end = self.editor.send(SCI_GETSELECTIONEND, 0, 0).max(0) as usize;
        self.editor.send(SCI_SETEMPTYSELECTION, end, 0);
        self.editor.search_anchor();
        match self.editor.search_next(query, flags.bits()) {
            -1 => None,
            pos => {
                self.center_caret_if_offscreen();
                Some(pos as u64)
            }
        }
    }

    fn search_prev(&mut self, query: &str, flags: SearchFlags) -> Option<u64> {
        let start = self.editor.send(SCI_GETSELECTIONSTART, 0, 0).max(0) as usize;
        self.editor.send(SCI_SETEMPTYSELECTION, start, 0);
        self.editor.search_anchor();
        match self.editor.search_prev(query, flags.bits()) {
            -1 => None,
            pos => {
                self.center_caret_if_offscreen();
                Some(pos as u64)
            }
        }
    }

    fn replace_current(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> bool {
        if query.is_empty() {
            return false;
        }
        let sel_start = self.editor.send(SCI_GETSELECTIONSTART, 0, 0).max(0) as u64;
        let sel_end = self.editor.send(SCI_GETSELECTIONEND, 0, 0).max(0) as u64;
        if sel_start == sel_end {
            return false;
        }
        // Only replace if the *selection itself* matches — the user may
        // have reselected arbitrary text since the last Find, and
        // Scintilla will not check that for us.
        self.editor.set_search_flags(flags.bits());
        self.editor.set_target_range(sel_start, sel_end);
        if self.editor.search_in_target(query) < 0 {
            return false;
        }
        let _ = self
            .editor
            .replace_target_with(replacement, flags.contains(SearchFlags::REGEX));
        let new_end = self.editor.target_end();
        self.editor
            .send(SCI_SETSELECTIONSTART, sel_start as usize, 0);
        self.editor.send(SCI_SETSELECTIONEND, new_end as usize, 0);
        true
    }

    fn replace_all(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> usize {
        if query.is_empty() {
            return 0;
        }
        self.editor.set_search_flags(flags.bits());
        // One undo group so the whole Replace All reverses in a single
        // Ctrl+Z, as the user expects.
        self.editor.send(SCI_BEGINUNDOACTION, 0, 0);
        let mut count = 0usize;
        let mut cursor = 0u64;
        loop {
            let doc_len = self.editor.send(SCI_GETLENGTH, 0, 0).max(0) as u64;
            self.editor.set_target_range(cursor, doc_len);
            if self.editor.search_in_target(query) < 0 {
                break;
            }
            let _ = self
                .editor
                .replace_target_with(replacement, flags.contains(SearchFlags::REGEX));
            let next = self.editor.target_end();
            // A zero-width match (`x*`, `^`, `\b`, …) with an empty
            // replacement leaves `target_end` exactly where the search
            // started. Without this step the same range is re-searched
            // forever and the UI thread wedges with no way out but a
            // kill. `count_matches` already guards this way; these two
            // loops did not, in either backend.
            cursor = if next > cursor {
                next
            } else {
                self.editor.send(SCI_POSITIONAFTER, next as usize, 0).max(0) as u64
            };
            count += 1;
        }
        self.editor.send(SCI_ENDUNDOACTION, 0, 0);
        count
    }

    fn count_matches(&mut self, query: &str, flags: SearchFlags) -> usize {
        if query.is_empty() {
            return 0;
        }
        self.editor.set_search_flags(flags.bits());
        let doc_len = self.editor.send(SCI_GETLENGTH, 0, 0).max(0) as u64;
        let mut count = 0usize;
        let mut cursor = 0u64;
        while cursor < doc_len {
            self.editor.set_target_range(cursor, doc_len);
            if self.editor.search_in_target(query) < 0 {
                break;
            }
            count += 1;
            let next = self.editor.target_end();
            // A zero-width match would leave `cursor` unchanged and spin
            // forever; step past it explicitly.
            cursor = if next > cursor {
                next
            } else {
                self.editor.send(SCI_POSITIONAFTER, next as usize, 0).max(0) as u64
            };
        }
        count
    }

    fn search_next_in_range(
        &mut self,
        query: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> Option<u64> {
        if query.is_empty() || end <= start {
            return None;
        }
        self.editor.set_search_flags(flags.bits());
        let caret = self.editor.send(SCI_GETSELECTIONEND, 0, 0).max(0) as u64;
        let lo = if caret >= start && caret < end {
            caret
        } else {
            start
        };
        self.editor.set_target_range(lo, end);
        if self.editor.search_in_target(query) < 0 {
            return None;
        }
        let pos = self.editor.target_start();
        let match_end = self.editor.target_end();
        self.editor.send(SCI_SETSELECTIONSTART, pos as usize, 0);
        self.editor.send(SCI_SETSELECTIONEND, match_end as usize, 0);
        self.center_caret_if_offscreen();
        Some(pos)
    }

    fn search_prev_in_range(
        &mut self,
        query: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> Option<u64> {
        if query.is_empty() || end <= start {
            return None;
        }
        self.editor.set_search_flags(flags.bits());
        let caret = self.editor.send(SCI_GETSELECTIONSTART, 0, 0).max(0) as u64;
        let upper = if caret > start && caret <= end {
            caret
        } else {
            end
        };
        // Scintilla has no "search backwards within a target range", so
        // walk forwards keeping the last hit.
        let mut last: Option<(u64, u64)> = None;
        let mut cursor = start;
        while cursor < upper {
            self.editor.set_target_range(cursor, upper);
            if self.editor.search_in_target(query) < 0 {
                break;
            }
            let pos = self.editor.target_start();
            let me = self.editor.target_end();
            last = Some((pos, me));
            cursor = if me > cursor {
                me
            } else {
                self.editor.send(SCI_POSITIONAFTER, me as usize, 0).max(0) as u64
            };
        }
        let (pos, match_end) = last?;
        self.editor.send(SCI_SETSELECTIONSTART, pos as usize, 0);
        self.editor.send(SCI_SETSELECTIONEND, match_end as usize, 0);
        self.center_caret_if_offscreen();
        Some(pos)
    }

    fn replace_all_in_range(
        &mut self,
        query: &str,
        replacement: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> (usize, u64) {
        if query.is_empty() || end <= start {
            return (0, end);
        }
        self.editor.set_search_flags(flags.bits());
        self.editor.send(SCI_BEGINUNDOACTION, 0, 0);
        let mut count = 0usize;
        let mut cursor = start;
        let mut range_end = end;
        loop {
            self.editor.set_target_range(cursor, range_end);
            if self.editor.search_in_target(query) < 0 {
                break;
            }
            let match_start = self.editor.target_start();
            let match_end = self.editor.target_end();
            let _ = self
                .editor
                .replace_target_with(replacement, flags.contains(SearchFlags::REGEX));
            let new_target_end = self.editor.target_end();
            // Same zero-width guard as `replace_all` above: a match
            // that does not advance would pin `cursor` and spin.
            let advanced_end = if new_target_end > cursor {
                new_target_end
            } else {
                self.editor
                    .send(SCI_POSITIONAFTER, new_target_end as usize, 0)
                    .max(0) as u64
            };
            // Every replacement shifts the range's far edge by the
            // length difference; the caller needs the corrected end to
            // keep its own bookkeeping in sync.
            let actual_replacement_len = new_target_end.saturating_sub(match_start);
            let delta = actual_replacement_len as i64 - (match_end as i64 - match_start as i64);
            cursor = advanced_end;
            range_end = (range_end as i64 + delta).max(cursor as i64) as u64;
            count += 1;
            if cursor >= range_end {
                break;
            }
        }
        self.editor.send(SCI_ENDUNDOACTION, 0, 0);
        (count, range_end)
    }

    // --- Chrome visibility -------------------------------------------
    //
    // The tab strip is real as of m3, so its pair below reports and
    // toggles the live widget. There is still no toolbar, so that pair
    // reports permanently hidden and refuses to change — an honest
    // answer to `NPPM_ISTOOLBARHIDDEN`, since the bar genuinely is not
    // shown, and returning the previous state unchanged tells a caller
    // nothing happened, which is the trait's documented signal.

    fn is_tabbar_hidden(&self) -> bool {
        !self.tabs.notebook.is_visible()
    }

    fn set_tabbar_hidden(&mut self, hidden: bool) -> bool {
        let prev = !self.tabs.notebook.is_visible();
        self.tabs.notebook.set_visible(!hidden);
        prev
    }

    fn is_toolbar_hidden(&self) -> bool {
        true
    }

    fn set_toolbar_hidden(&mut self, _hidden: bool) -> bool {
        true
    }

    fn is_menu_hidden(&self) -> bool {
        !self.menu_bar.is_visible()
    }

    fn set_menu_hidden(&mut self, hidden: bool) -> bool {
        let prev = !self.menu_bar.is_visible();
        self.menu_bar.set_visible(!hidden);
        prev
    }

    fn is_statusbar_hidden(&self) -> bool {
        !self.status.container.is_visible()
    }

    fn set_statusbar_hidden(&mut self, hidden: bool) -> bool {
        let prev = !self.status.container.is_visible();
        self.status.container.set_visible(!hidden);
        prev
    }

    fn editor_zoom_level(&self) -> i32 {
        self.editor.send(SCI_GETZOOM, 0, 0) as i32
    }

    fn editor_default_fg_color(&self) -> i32 {
        self.editor.send(SCI_STYLEGETFORE, STYLE_DEFAULT, 0) as i32
    }

    fn editor_default_bg_color(&self) -> i32 {
        self.editor.send(SCI_STYLEGETBACK, STYLE_DEFAULT, 0) as i32
    }

    fn set_smooth_font(&mut self, _smooth: bool) -> bool {
        // `SCI_SETFONTQUALITY` is a no-op outside the Win32 backend —
        // GTK renders through cairo/pango, whose antialiasing comes
        // from the desktop's font settings, not from Scintilla. Report
        // "was already smooth" rather than claiming a change we did not
        // make.
        true
    }

    fn set_editor_border_edge(&mut self, enable: bool) -> bool {
        // Win32 toggles WS_EX_CLIENTEDGE. The GTK analogue is a frame
        // shadow on the scrolled container; m2 packs the view directly
        // into the layout box, so there is nothing to toggle yet.
        tracing::trace!(enable, "NPPM_SETEDITORBORDEREDGE: no GTK equivalent in m2");
        false
    }

    fn set_line_number_width_mode(&mut self, mode: i32) -> bool {
        tracing::trace!(
            mode,
            "NPPM_SETLINENUMBERWIDTHMODE: recorded, not yet applied"
        );
        true
    }

    fn capture_text_from_doc(&mut self, scintilla_doc: isize) -> String {
        self.with_doc(scintilla_doc, |s| read_all(&s.editor), String::new())
    }

    fn is_doc_dirty(&mut self, scintilla_doc: isize) -> bool {
        self.with_doc(
            scintilla_doc,
            |s| s.editor.send(SCI_GETMODIFY, 0, 0) != 0,
            false,
        )
    }

    fn replace_doc_text(&mut self, doc: isize, text: &str) -> bool {
        if doc == 0 {
            return false;
        }
        self.with_doc(
            doc,
            |s| {
                let mut bytes = Vec::with_capacity(text.len() + 1);
                bytes.extend_from_slice(text.as_bytes());
                bytes.push(0);
                // `SCI_SETTEXT` alone, deliberately. `set_buffer_text`
                // follows it with `SCI_EMPTYUNDOBUFFER` and
                // `SCI_SETSAVEPOINT`, which is right when installing
                // freshly-loaded file content and wrong here: the user
                // must be able to undo a Replace-in-Files, and the
                // buffer must read as modified so the change is not
                // lost when the tab closes.
                s.editor.send(SCI_SETTEXT, 0, bytes.as_ptr() as isize);
                true
            },
            false,
        )
    }

    fn mark_active_buffer_dirty(&mut self) {
        if self.editor.send(SCI_GETMODIFY, 0, 0) != 0 {
            return;
        }
        // An empty undo action is the documented way to move a buffer
        // off its save point without changing a byte of text.
        self.editor.send(SCI_ADDUNDOACTION, 0, 0);
    }
}

/// Regression tests for the doc-pointer discipline that makes one
/// Scintilla view serve many tabs.
///
/// These exist because the first cut of this backend got it wrong in a
/// way that silently corrupted user files: `Shell` moves `active_tab`
/// synchronously for an already-open path and for a tab close, but the
/// view was left bound to the *previous* tab's document. Since
/// `get_buffer_text` reads whatever is bound while the save path takes
/// the path from whatever is active, Ctrl+S then wrote one buffer's
/// bytes over a different file. The invariant asserted below —
/// "`activate_tab(doc)` means the very next read sees `doc`" — is the
/// one that has to hold for `rebind_active_view` to be correct.
///
/// Requires a display, for the same reason `scintilla-sys`'s FFI smoke
/// test does: `scintilla_new` builds a `GtkWidget`. `#[ignore]` rather
/// than a runtime probe so a headless CI run reports it skipped instead
/// of silently passing. Run with:
///
/// ```text
/// cargo test -p codepp-ui-gtk -- --ignored
/// xvfb-run cargo test -p codepp-ui-gtk -- --ignored
/// ```
///
/// **One test function, deliberately.** GTK is single-threaded, and
/// cargo runs test functions on separate threads by default — a second
/// function touching GTK concurrently segfaults inside GDK. Splitting
/// these would mean relying on every future runner remembering
/// `--test-threads=1`, so the scenarios are sequenced inside one
/// function instead.
#[cfg(test)]
mod doc_binding_tests {
    use super::{GtkUi, UiPlatform};
    use crate::status::StatusBar;
    use crate::tabs::TabStrip;
    use codepp_editor::EditorHandle;
    use codepp_scintilla_sys::scintilla_new;
    use gtk::glib::translate::FromGlibPtrNone;

    /// Build a real editor over a real Scintilla widget.
    fn fixture() -> GtkUi {
        gtk::init().expect("gtk::init failed — no display?");
        // SAFETY: GTK is initialised, `scintilla_new`'s only
        // precondition.
        let ptr = unsafe { scintilla_new() };
        assert!(!ptr.is_null(), "scintilla_new returned null");
        // Leak a reference for the test's duration, exactly as
        // `GtkUiState.sci_widget` holds one in the real program — the
        // `EditorHandle` below carries raw pointers into this widget.
        let widget: gtk::Widget =
            unsafe { gtk::Widget::from_glib_none(ptr.cast::<gtk::ffi::GtkWidget>()) };
        std::mem::forget(widget);
        // SAFETY: `ptr` is the live widget leaked just above.
        let editor: EditorHandle =
            unsafe { EditorHandle::from_gtk_widget(ptr) }.expect("no direct-call pair");
        GtkUi {
            window: gtk::Window::new(gtk::WindowType::Toplevel),
            editor,
            status: StatusBar::new(),
            menu_bar: gtk::MenuBar::new(),
            tabs: TabStrip::new(),
        }
    }

    #[test]
    #[ignore = "creates a GTK widget; needs a display (see module docs)"]
    fn view_binding_follows_the_requested_document() {
        let mut ui = fixture();

        let doc_a = ui.activate_tab(0, 0);
        ui.set_buffer_text("AAAA-file-A-contents", 0);
        let doc_b = ui.activate_tab(1, 0);
        ui.set_buffer_text("BBBB-file-B-contents", 0);
        assert_ne!(doc_a, doc_b, "each tab must get its own document");

        // The view is on B, which is what a read must return.
        assert_eq!(ui.get_buffer_text(), "BBBB-file-B-contents");

        // Switching back to an already-materialised document — the
        // exact step `rebind_active_view` performs after
        // `SwitchedToExisting` or a tab close. Before the fix this step
        // was missing at those call sites, and this is the assertion
        // that would have failed: a save of "tab A" wrote B's bytes.
        assert_eq!(ui.activate_tab(0, doc_a), doc_a);
        assert_eq!(
            ui.get_buffer_text(),
            "AAAA-file-A-contents",
            "after rebinding to A, reads must see A — not the previously bound buffer"
        );

        // Reading another tab's text while the view sits on A is what
        // Save All does...
        assert_eq!(ui.capture_text_from_doc(doc_b), "BBBB-file-B-contents");
        // ...and it must put the view back, or the user would find
        // themselves looking at a different buffer than before.
        assert_eq!(
            ui.get_buffer_text(),
            "AAAA-file-A-contents",
            "capture_text_from_doc must restore the previously bound document"
        );
        // A never-materialised document reads as empty, not as whatever
        // happens to be bound.
        assert_eq!(ui.capture_text_from_doc(0), "");

        tab_strip_scenarios(&ui);
        single_view_invariant(&ui);
        regex_replace_expands_groups(&mut ui);
    }

    /// `replace_all` with the REGEX flag must expand `\1` against the
    /// match; without it, the replacement is literal.
    ///
    /// This is the end-to-end check behind the `replace_target_with`
    /// change: the flag has to reach `SCI_REPLACETARGETRE` for the
    /// group to expand, and `SCI_REPLACETARGET` for it to stay literal.
    /// Only a real Scintilla can prove that, since the substitution
    /// happens inside vendored C++.
    fn regex_replace_expands_groups(ui: &mut GtkUi) {
        use codepp_shell::SearchFlags;

        // Regex mode: `\1` is the captured digits.
        ui.activate_tab(90, 0);
        ui.set_buffer_text("item12 = x\nitem34 = y\n", 0);
        let n = ui.replace_all(r"item(\d+)", r"key_\1", SearchFlags::REGEX);
        assert_eq!(n, 2, "both lines match");
        assert_eq!(
            ui.get_buffer_text(),
            "key_12 = x\nkey_34 = y\n",
            "regex replace must expand the capture group, not insert `\\1` literally"
        );

        // Literal mode: `\1` is two characters, and the query is not a
        // pattern — so nothing matches `item(\d+)` as literal text.
        ui.activate_tab(91, 0);
        ui.set_buffer_text("a\\1b\n", 0);
        let n = ui.replace_all(r"\1", "Z", SearchFlags::NONE);
        assert_eq!(n, 1, "the literal two-char string `\\1` occurs once");
        assert_eq!(
            ui.get_buffer_text(),
            "aZb\n",
            "literal replace must treat `\\1` as text, not a group reference"
        );
    }

    /// Exercises the pattern the single-view model makes safe: a
    /// handle copied *before* a burst of tab-like document churn is
    /// still usable after it, because only documents were created and
    /// released — the view underneath was not.
    ///
    /// Be clear about what this does **not** cover. It cannot detect a
    /// future change that destroys the view, because that would fault
    /// inside vendored C++ rather than fail an assertion. The check
    /// that catches *that* is the source-level one in
    /// `single_view_source_invariant`, which is why both exist.
    fn single_view_invariant(ui: &GtkUi) {
        // A copy, taken before the churn — the shape that goes wrong.
        let stale_copy = ui.editor;
        let mut churn = GtkUi { ..fixture_from(ui) };
        let mut docs = Vec::new();
        for i in 0..8 {
            let doc = churn.activate_tab(i, 0);
            churn.set_buffer_text(&format!("buffer {i}"), 0);
            docs.push(doc);
        }
        // Release them all, as closing every tab would.
        for doc in docs {
            if doc != 0 {
                churn
                    .editor
                    .send(codepp_scintilla_sys::SCI_RELEASEDOCUMENT, 0, doc);
            }
        }
        // The copy taken before all of that must still be usable,
        // because the *view* was never destroyed — only documents were.
        let len = stale_copy.send(codepp_scintilla_sys::SCI_GETLENGTH, 0, 0);
        assert!(
            len >= 0,
            "a handle copied before document churn must still address a live view"
        );
    }

    /// Rebuild a `GtkUi` around the same editor, mirroring what
    /// `GtkUiState::split` hands out per drain.
    fn fixture_from(ui: &GtkUi) -> GtkUi {
        GtkUi {
            window: ui.window.clone(),
            editor: ui.editor,
            status: ui.status.clone(),
            menu_bar: ui.menu_bar.clone(),
            tabs: ui.tabs.clone(),
        }
    }

    /// Tab-strip behaviour, sequenced inside the single GTK test
    /// function above rather than given its own `#[test]` — see the
    /// module docs for why a second concurrent GTK test segfaults.
    fn tab_strip_scenarios(ui: &GtkUi) {
        use codepp_shell::Tab;
        use gtk::prelude::*;
        use std::cell::Cell;
        use std::path::PathBuf;
        use std::rc::Rc;

        let strip = &ui.tabs;
        let tab = |name: &str| Tab {
            path: Some(PathBuf::from(format!("/tmp/{name}"))),
            ..Tab::default()
        };

        // Count any `switch-page` that arrives *without* the sync guard
        // set. Every one of those would re-enter `Shell` and move
        // `active_tab` behind the user's back — the specific trap the
        // suppression exists for, and the one thing here that a
        // reviewer cannot verify by reading.
        let leaked = Rc::new(Cell::new(0usize));
        let leaked_probe = Rc::clone(&leaked);
        strip.notebook.connect_switch_page(move |_, _, _| {
            if !crate::tabs::is_suppressed() {
                leaked_probe.set(leaked_probe.get() + 1);
            }
        });

        // Grow.
        let tabs = vec![tab("a.rs"), tab("b.rs"), tab("c.rs")];
        strip.sync(&tabs, Some(0));
        assert_eq!(strip.notebook.n_pages(), 3, "one page per tab");
        assert_eq!(strip.notebook.current_page(), Some(0));

        // Select a different tab.
        strip.sync(&tabs, Some(2));
        assert_eq!(strip.notebook.current_page(), Some(2));

        // Shrink. `remove_page` shifts the selection without emitting
        // `switch-page`, which is exactly why `sync` sets the selection
        // explicitly instead of trusting a signal.
        let fewer = vec![tab("a.rs")];
        strip.sync(&fewer, Some(0));
        assert_eq!(strip.notebook.n_pages(), 1, "pages follow the model down");
        assert_eq!(strip.notebook.current_page(), Some(0));

        // Empty.
        strip.sync(&[], None);
        assert_eq!(strip.notebook.n_pages(), 0);

        // An out-of-range active index must not panic or select
        // anything — `Shell` and the strip can disagree for one call
        // while a load is in flight.
        strip.sync(&fewer, Some(99));
        assert_eq!(strip.notebook.n_pages(), 1);

        // Not one unsuppressed `switch-page` across all of the above.
        // Without the guard, the first `append_page` and every
        // `set_current_page` would each have produced one.
        assert_eq!(
            leaked.get(),
            0,
            "sync leaked switch-page signals; handlers would re-enter Shell"
        );

        // The reorder handler's pre-drag lookup. `sync` records page
        // order, so each page reports the index it currently occupies;
        // an unknown widget reports nothing rather than a wrong index.
        let three = vec![tab("a.rs"), tab("b.rs"), tab("c.rs")];
        strip.sync(&three, Some(0));
        for i in 0..3u32 {
            let page = strip.notebook.nth_page(Some(i)).expect("page exists");
            assert_eq!(strip.index_before_reorder(&page), Some(i as usize));
        }
        let stranger: gtk::Widget = gtk::Label::new(None).upcast();
        assert_eq!(strip.index_before_reorder(&stranger), None);

        // Labels carry the display name, and are rebuilt from the model
        // on every sync — a renamed tab must not keep its old label.
        let renamed = vec![Tab {
            custom_name: Some("renamed".to_string()),
            ..tab("a.rs")
        }];
        strip.sync(&renamed, Some(0));
        let page = strip.notebook.nth_page(Some(0)).expect("page exists");
        let label = strip.notebook.tab_label(&page).expect("tab has a label");
        assert!(
            label_text(&label).is_some_and(|t| t == "renamed"),
            "label should show the resolved display name"
        );
    }

    /// Depth-first search for the first `GtkLabel`'s text inside a tab
    /// label widget, which is an `EventBox` wrapping a box of icon +
    /// label + close button.
    fn label_text(widget: &gtk::Widget) -> Option<String> {
        use gtk::prelude::*;
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            return Some(label.text().to_string());
        }
        let container = widget.downcast_ref::<gtk::Container>()?;
        container.children().iter().find_map(label_text)
    }
}
