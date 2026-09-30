//! `impl UiPlatform for CocoaUi` — the shell's view of this backend.
//!
//! Almost everything here is Scintilla work rather than Cocoa work, and
//! is therefore a close port of `ui_gtk`'s equivalent: the doc-pointer
//! swap that lets one view serve many tabs, the search/replace drivers,
//! and the status-bar refresh are all sequences of `EditorHandle::send`
//! with no toolkit involvement. Where a method genuinely needs the
//! toolkit — window transparency, chrome visibility — the Cocoa answer
//! is noted inline against what Win32 and GTK do.
//!
//! Keeping the three implementations textually parallel is deliberate.
//! DESIGN.md §7.5 makes parity checkable by comparison, and a Scintilla
//! call sequence that drifts between backends is a bug the user sees as
//! "the same file behaves differently on my Mac".

use codepp_core::styles::{parse_rgb_hex, Styles};
use codepp_core::{Encoding, Eol, LangType};
use codepp_editor::EditorHandle;
use codepp_scintilla_sys::{
    SCI_ADDUNDOACTION, SCI_BEGINUNDOACTION, SCI_COLOURISE, SCI_CONVERTEOLS, SCI_CREATEDOCUMENT,
    SCI_EMPTYUNDOBUFFER, SCI_ENDUNDOACTION, SCI_GETANCHOR, SCI_GETCOLUMN, SCI_GETCURRENTPOS,
    SCI_GETDOCPOINTER, SCI_GETFIRSTVISIBLELINE, SCI_GETLENGTH, SCI_GETLINECOUNT, SCI_GETMODIFY,
    SCI_GETOVERTYPE, SCI_GETSELECTIONEND, SCI_GETSELECTIONSTART, SCI_GETTEXT, SCI_GETXOFFSET,
    SCI_GETZOOM, SCI_GOTOPOS, SCI_LINEFROMPOSITION, SCI_LINESCROLL, SCI_LINESONSCREEN,
    SCI_POSITIONAFTER, SCI_RELEASEDOCUMENT, SCI_SETDOCPOINTER, SCI_SETEMPTYSELECTION,
    SCI_SETEOLMODE, SCI_SETSAVEPOINT, SCI_SETSEL, SCI_SETSELECTIONEND, SCI_SETSELECTIONSTART,
    SCI_SETTABWIDTH, SCI_SETTEXT, SCI_SETXOFFSET, SCI_STYLEGETBACK, SCI_STYLEGETFORE, SC_EOL_CR,
    SC_EOL_CRLF, SC_EOL_LF, STYLE_DEFAULT,
};
use codepp_shell::{ClipboardData, SearchFlags, UiPlatform};
use objc2_app_kit::{
    NSPasteboard, NSPasteboardTypeHTML, NSPasteboardTypeRTF, NSPasteboardTypeString,
};
use objc2_foundation::{NSData, NSPoint, NSRect, NSSize};

use crate::state::CocoaUi;

/// Scintilla's default document options, for `SCI_CREATEDOCUMENT`.
const SC_DOCUMENTOPTION_DEFAULT: isize = 0;

/// Tab width in columns. Matches the other two backends — Scintilla's
/// own default is 8, which would make the same file render differently
/// on macOS than on Windows.
const TAB_WIDTH_SPACES: usize = 4;

/// Index of the line-number margin. Same slot the other backends use.
const LINE_NUMBER_MARGIN: u32 = 0;
/// Margin index for the change-history "edit indicator" strip. 4 sits to
/// the right of the line-number margin (and a future fold margin), the
/// same slot `ui_win32` and `ui_gtk` use so all three backends match.
const CHANGE_HISTORY_MARGIN: u32 = 4;
/// Pixel width of the change-history strip when populated — a thin
/// slice, matching the other backends.
const CHANGE_HISTORY_MARGIN_PX: i32 = 4;
/// Change-history strip colour: Material orange 400 in Scintilla's
/// `0x00BBGGRR` order, the same shade the active-tab indicator uses.
const CHANGE_HISTORY_COLOR: u32 = 0x00_26_A7_FF;

/// Pack an `(r, g, b)` triple into Scintilla's BGR colour word.
const fn rgb_to_scintilla_colour((r, g, b): (u8, u8, u8)) -> u32 {
    (b as u32) << 16 | (g as u32) << 8 | (r as u32)
}

thread_local! {
    /// Last clamped line-number digit count, so the margin is only
    /// re-measured when it actually changes. See `refresh_dynamic_status`.
    static LAST_LINE_NUMBER_DIGITS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Caret and viewport state, saved across a temporary document swap.
struct ViewState {
    caret: isize,
    anchor: isize,
    top_line: isize,
    x_offset: isize,
}

impl CocoaUi {
    /// Re-lay the content view after a chrome strip was shown or hidden.
    ///
    /// The autoresizing masks handle a *window resize* — one flexible
    /// editor between fixed strips — but they say nothing about a strip
    /// becoming invisible, because a hidden view keeps its frame. Without
    /// this, hiding the toolbar or the tab strip leaves a blank band where
    /// it was.
    ///
    /// Win32 does the same thing (`relayout_main_window_via_post` after
    /// its `ShowWindow`). An earlier version of this backend's comment
    /// claimed Win32 left the gap; that was simply wrong.
    ///
    /// **A method on `CocoaUi`, deliberately, not a free function reaching
    /// through `with_state`.** Every `UiPlatform` method already runs
    /// inside a `with_state` borrow, so a nested one is declined and a
    /// free function would silently never run — measured: the first
    /// version was written that way and the editor's frame did not move
    /// on any of the four hide/show calls in a driven test. Everything
    /// this touches is therefore a field of `self`.
    ///
    /// Recomputed from scratch rather than adjusted by a delta, so it is
    /// correct however many strips are hidden and in whatever order they
    /// were toggled.
    pub(crate) fn relayout_chrome(&self) {
        let Some(content) = self.window.contentView() else {
            return;
        };
        let size = content.bounds().size;
        let toolbar_h = if self.toolbar.is_hidden() {
            0.0
        } else {
            crate::toolbar::TOOLBAR_HEIGHT
        };
        let status_h = crate::status::STATUS_BAR_HEIGHT;
        // Bottom-up, in Cocoa's unflipped content coordinates: status
        // bar, dock area, toolbar. The dock area absorbs what is left,
        // floored at zero so a very short window cannot ask for a
        // negative height.
        let area_h = (size.height - status_h - toolbar_h).max(0.0);
        self.status.container.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(size.width, status_h),
        ));
        self.dock_area.setFrame(NSRect::new(
            NSPoint::new(0.0, status_h),
            NSSize::new(size.width, area_h),
        ));
        self.toolbar.container.setFrame(NSRect::new(
            NSPoint::new(0.0, status_h + area_h),
            NSSize::new(size.width, toolbar_h),
        ));
        // Carve the area into side bands and the editor cell — the dock
        // model's job (`core::dock::compute_frame`), applied by the
        // Cocoa mechanism. Takes only the dock borrow, which is why it
        // can run from inside this `with_state` borrow; it sets the
        // editor cell's frame, which the rest of this method lays out
        // inside of.
        crate::dock::layout_area(size.width, area_h);
        // Inside the cell, bottom-up: results dock, editor, tab strip.
        // The status bar and toolbar are outside the cell now, so the
        // dock's clamp is against the cell alone.
        let cell = self.editor_cell.bounds().size;
        let tabs_h = if self.tabs.is_hidden() {
            0.0
        } else {
            crate::tabs::TAB_STRIP_HEIGHT
        };
        // Zero while the dock is closed, which is most of the time, and
        // otherwise clamped to what this cell can give it — see
        // `FifDock::height_for_layout`.
        let dock_h = self.fif_dock.height_for_layout(cell.height, tabs_h);
        let editor_h = (cell.height - dock_h - tabs_h).max(0.0);
        self.fif_dock.container.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(cell.width, dock_h),
        ));
        self.sci_view.setFrame(NSRect::new(
            NSPoint::new(0.0, dock_h),
            NSSize::new(cell.width, editor_h),
        ));
        self.tabs.container.setFrame(NSRect::new(
            NSPoint::new(0.0, dock_h + editor_h),
            NSSize::new(cell.width, tabs_h),
        ));
    }

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
    /// therefore visible to the *view*, not just the model, which is why
    /// the caret and scroll offsets have to be saved and put back —
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
        // The line-number margin holds a fixed *minimum* width and only
        // grows for files past the digit budget. This fires on every
        // notification — caret moves included — so gate the actual
        // `SCI_TEXTWIDTH` re-measure on the *clamped* digit count
        // changing: within the budget the clamp pins it to the floor, so
        // nothing re-measures on ordinary edits; only crossing
        // 99 999 → 100 000 (or higher) moves it. Same shape as GTK's.
        let digits = codepp_editor::line_number_digits(lines.max(1))
            .max(codepp_editor::LINE_NUMBER_MARGIN_DIGITS);
        if LAST_LINE_NUMBER_DIGITS.with(|c| c.replace(digits)) != digits {
            self.editor.update_line_number_width(LINE_NUMBER_MARGIN);
        }
    }
}

/// Configure the predefined 32-39 styles that `SCI_STYLECLEARALL`
/// resets — the line-number margin, the change-history strip's
/// definitions, the brace-highlight pair, and the indent-guide colour.
pub(crate) fn apply_predefined_styles(editor: &EditorHandle) {
    // Styles STYLE_LINENUMBER (fore/back) and configures margin 0 as
    // Scintilla's built-in `SC_MARGIN_NUMBER` at the shared
    // fixed-minimum width — identical on all three backends.
    codepp_editor::theme::apply_line_number_margin(editor);
    // The change-history "edit indicator" strip. Shared config, so it
    // looks and behaves identically on all three backends;
    // per-document *enablement* happens in `activate_tab`.
    //
    // Without this call the margin still appears, because
    // `SC_CHANGE_HISTORY_MARKERS` is enabled per document — but it
    // renders with Scintilla's *built-in* marker definitions, which draw
    // an outlined bar with a lighter fill rather than the solid strip
    // Win32 and GTK show. That was the visible symptom of this call
    // being missing: same feature, different appearance, on macOS only.
    editor.configure_change_history_margin(
        CHANGE_HISTORY_MARGIN,
        CHANGE_HISTORY_MARGIN_PX,
        CHANGE_HISTORY_COLOR,
    );
    // Both were likewise missing here while present on the other two
    // backends, so brace matching and indent guides rendered with
    // Scintilla's defaults instead of Code++'s palette.
    codepp_editor::theme::apply_brace_styles(editor);
    codepp_editor::theme::apply_indent_guide_style(editor);
}

/// Read the whole buffer out of Scintilla as a `String`.
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
    // Scintilla stores bytes, not validated UTF-8: a file that failed to
    // decode cleanly can leave invalid sequences in the buffer. Lossy
    // conversion keeps the editor usable instead of panicking.
    String::from_utf8_lossy(&buf).into_owned()
}

/// Map [`Eol`] to Scintilla's `SC_EOL_*` code. One place, so
/// `update_status`'s insert mode and `convert_doc_eols`'s target
/// cannot disagree on where `Mixed` lands: it has no Scintilla
/// equivalent, and LF is the least surprising ending for new lines —
/// matching `Eol::bytes()` and the other two backends.
fn sc_eol_for(eol: Eol) -> usize {
    match eol {
        Eol::CrLf => SC_EOL_CRLF,
        Eol::Cr => SC_EOL_CR,
        Eol::Lf | Eol::Mixed => SC_EOL_LF,
    }
}

impl UiPlatform for CocoaUi {
    fn activate_tab(&mut self, _idx: usize, scintilla_doc: isize) -> isize {
        // 0 means "this tab has no document yet" — mint one. Every other
        // value is a live doc pointer from a previous call.
        let fresh = scintilla_doc == 0;
        let doc = if fresh {
            self.editor
                .send(SCI_CREATEDOCUMENT, 0, SC_DOCUMENTOPTION_DEFAULT)
        } else {
            scintilla_doc
        };
        // Skip the swap when this doc is already bound. `SCI_SETDOCPOINTER`
        // clears the caret to 0 on every bind — even a redundant re-point
        // at the current document — so avoiding the no-op swap preserves
        // the caret whenever the view already shows the target doc. A
        // fresh doc is never the current one, so it always binds.
        if doc != self.editor.send(SCI_GETDOCPOINTER, 0, 0) {
            self.editor.send(SCI_SETDOCPOINTER, 0, doc);
        }
        if fresh {
            // Tab width is *per-document* state in Scintilla, so it has
            // to be set on each new document rather than once at
            // startup. Without this a macOS buffer would render tabs at
            // Scintilla's built-in 8 columns while the other backends
            // use 4 — the same file looking different per platform.
            self.editor.send(SCI_SETTABWIDTH, TAB_WIDTH_SPACES, 0);
            // Change-history tracking is per-document too: every fresh
            // `SCI_CREATEDOCUMENT` starts with it off.
            self.editor.enable_change_history();
        }
        doc
    }

    fn set_buffer_text(&mut self, text: &str, cursor: u64) {
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        self.editor.send(SCI_SETTEXT, 0, bytes.as_ptr() as isize);
        // A freshly loaded file is not an edit: drop the undo history the
        // `SETTEXT` itself created and mark the buffer clean, or the user
        // could ⌘Z their file back to empty.
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
        // Keep Scintilla's own EOL mode in step, so newly typed lines use
        // the same ending as the rest of the file.
        self.editor.send(SCI_SETEOLMODE, sc_eol_for(eol), 0);
        // A UDL's own `<UserLang name>` for a UDL id, the built-in name
        // otherwise. Resolved through the registry pointer rather than
        // `with_state`, because this runs inside a live borrow — see the
        // `CocoaUi::udl_registry` field doc.
        //
        // SAFETY: `self.udl_registry` is the pointer `CocoaUiState::split`
        // captured from `Shell.udl_registry`, read-only, per that doc.
        let lang_label = crate::udl::resolve_lang_label(lang, self.udl_registry);
        self.status
            .set_static_parts(&lang_label, eol.long_label(), encoding.label());
        self.refresh_dynamic_status();
    }

    fn set_plugin_status(&mut self, section: usize, text: &str) {
        self.status.set_plugin_part(section, text);
    }

    fn mark_saved(&mut self) {
        self.editor.send(SCI_SETSAVEPOINT, 0, 0);
    }

    fn apply_lang(&mut self, lang: LangType) {
        // A UDL has no Lexilla lexer, so it takes the container-lexer
        // path instead of the theme table: `SCLEX_CONTAINER` plus its own
        // palette, then `SCN_STYLENEEDED` drives the host-side tokeniser
        // (see `crate::udl::on_style_needed`). Returns `false` for every
        // built-in id, which falls through to the shared table below.
        //
        // SAFETY: `self.udl_registry` is the pointer `CocoaUiState::split`
        // captured from `Shell.udl_registry`, read-only, per its doc.
        if crate::udl::apply_lang(&self.editor, self.udl_registry, lang) {
            return;
        }
        // The shared Lexilla theme table, exactly as GTK uses it. Both
        // branches route through `apply_default_styles`, whose
        // `apply_line_number_margin` re-configures the built-in number
        // margin after the style clear — no per-backend fixup needed.
        codepp_editor::theme::apply_lang_theme(&self.editor, lang);
    }

    fn apply_default_style(&mut self, styles: &Styles) {
        let entry = styles.effective_default();
        // Same fallbacks as the other backends: black on white if the
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

        // Win32 applies window transparency via `WS_EX_LAYERED` and GTK
        // via the toplevel's opacity; `NSWindow.alphaValue` is the direct
        // Cocoa equivalent and needs no compositor cooperation. `Styles::clamp`
        // already floors `percent` to the documented 20..=100 range on both
        // the load and write paths; the clamp here is defence-in-depth
        // against a `Styles` that never went through it, and states the
        // field's invariant at the point of use.
        let transparency = styles.effective_transparency();
        self.window.setAlphaValue(if transparency.enabled {
            f64::from(transparency.percent.clamp(
                codepp_core::styles::TRANSPARENCY_PERCENT_MIN,
                codepp_core::styles::TRANSPARENCY_PERCENT_MAX,
            )) / 100.0
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
        // ⌘Z, as the user expects.
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
            // kill.
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
            // Same zero-width guard as `replace_all` above.
            let advanced_end = if new_target_end > cursor {
                new_target_end
            } else {
                self.editor
                    .send(SCI_POSITIONAFTER, new_target_end as usize, 0)
                    .max(0) as u64
            };
            // Every replacement shifts the range's far edge by the length
            // difference; the caller needs the corrected end to keep its
            // own bookkeeping in sync.
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
    // The tab strip and status bar are real, so each pair reports and
    // toggles its live view; `set_*` returns the *previous* hidden state
    // the trait documents as its result. The toolbar is not built yet
    // (m3b), so its pair reports "not hidden" — accurate, since there is
    // nothing to hide.

    fn is_tabbar_hidden(&self) -> bool {
        self.tabs.is_hidden()
    }

    /// `NPPM_HIDETABBAR`. Returns the *previous* hidden state, which is
    /// the trait's contract and what the plugin sees.
    fn set_tabbar_hidden(&mut self, hidden: bool) -> bool {
        let was = self.tabs.is_hidden();
        self.tabs.set_hidden(hidden);
        // Give the freed band back to the editor rather than leaving a
        // gap — see [`CocoaUi::relayout_chrome`].
        self.relayout_chrome();
        was
    }

    fn is_toolbar_hidden(&self) -> bool {
        self.toolbar.is_hidden()
    }

    /// `NPPM_HIDETOOLBAR`, and the View menu once it grows the entry.
    ///
    /// Returns the **previous** hidden state, not whether the call
    /// succeeded. That is the trait's stated contract and it is what
    /// reaches the plugin as `NPPM_HIDETOOLBAR`'s return value, pinned by
    /// `plugin-host`'s own `hide_toolbar_returns_previous_and_flips`
    /// test — returning `true` unconditionally told every plugin the bar
    /// had already been hidden.
    fn set_toolbar_hidden(&mut self, hidden: bool) -> bool {
        let was = self.toolbar.is_hidden();
        self.toolbar.set_hidden(hidden);
        self.relayout_chrome();
        was
    }

    fn is_menu_hidden(&self) -> bool {
        // macOS has no per-app menu-bar hiding for an ordinary window —
        // the menu bar belongs to the system, not the app, and the only
        // thing resembling it is full-screen auto-hide, which is the
        // user's choice rather than the app's. Reporting "not hidden" is
        // the truthful answer rather than a stub.
        false
    }

    fn set_menu_hidden(&mut self, _hidden: bool) -> bool {
        // Declined, for the reason above. Emptying `NSApp.mainMenu`
        // would technically blank it, but it would also strip every
        // key equivalent — ⌘Q included — which is worse than declining.
        let _ = &self.menu;
        false
    }

    fn is_statusbar_hidden(&self) -> bool {
        self.status.is_hidden()
    }

    fn set_statusbar_hidden(&mut self, hidden: bool) -> bool {
        let was = self.status.is_hidden();
        self.status.set_hidden(hidden);
        was
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
        // No-op outside Win32, same as GTK. Cocoa's text rendering is
        // always antialiased and the choice is a system preference, not
        // an application one; accepting is more truthful than failing,
        // because the plugin's intent (smooth text) is already the case.
        true
    }

    fn set_editor_border_edge(&mut self, enable: bool) -> bool {
        // No Cocoa equivalent: `WS_EX_CLIENTEDGE` is a Win32 window
        // style. Declined rather than silently ignored, matching GTK.
        tracing::trace!(enable, "NPPM_SETEDITORBORDEREDGE: no Cocoa equivalent");
        false
    }

    fn set_line_number_width_mode(&mut self, mode: i32) -> bool {
        // `true`, matching Win32 and GTK. The trait's contract is
        // "was the *mode value* accepted", not "did the gutter visibly
        // change" — and the shell bridge already rejects unknown values
        // before delegating here, so by this point `mode` is always one
        // of the two documented constants. Returning `false` would tell
        // a plugin that even `LINENUMWIDTH_DYNAMIC` — the mode this
        // backend already behaves as — had failed, and it would do so on
        // macOS only, which is exactly the kind of silent per-platform
        // plugin divergence the ABI freeze exists to prevent.
        tracing::trace!(
            mode,
            "NPPM_SETLINENUMBERWIDTHMODE: accepted, width is dynamic"
        );
        true
    }

    fn capture_text_from_doc(&mut self, scintilla_doc: isize) -> String {
        self.with_doc(scintilla_doc, |ui| read_all(&ui.editor), String::new())
    }

    fn is_doc_dirty(&mut self, doc: isize) -> bool {
        self.with_doc(doc, |ui| ui.editor.send(SCI_GETMODIFY, 0, 0) != 0, false)
    }

    fn replace_doc_text(&mut self, doc: isize, text: &str) -> bool {
        self.with_doc(
            doc,
            |ui| {
                let mut bytes = Vec::with_capacity(text.len() + 1);
                bytes.extend_from_slice(text.as_bytes());
                bytes.push(0);
                // `SCI_SETTEXT` alone, deliberately — matching GTK.
                // It is already a single undoable action, so wrapping it
                // in a begin/end pair would add a nesting level without
                // changing what a ⌘Z reverses.
                ui.editor.send(SCI_SETTEXT, 0, bytes.as_ptr() as isize);
                true
            },
            false,
        )
    }

    fn release_doc(&mut self, doc: isize) {
        if doc == 0 {
            // "Never materialized" sentinel — nothing to release.
            return;
        }
        // Drops the tab-owned reference. A still-bound document only
        // goes 2→1 here (the view holds its own reference; the free
        // happens at the next `SCI_SETDOCPOINTER`); an unbound one is
        // freed immediately — the same shape `action_close_tab`'s
        // release relies on, and consistent with the `LAST_SEEDED_DOC`
        // ABA premise in `lib.rs`: nothing here frees a document a
        // pending bind is about to install. See the trait docs.
        //
        // One count on this backend the trait docs' 2→1 walkthrough
        // does not include: an open Document Map holds its *own*
        // `SCI_SETDOCPOINTER` reference to the active tab's document
        // (`docmap::sync_to_active_tab`), so the moment-of-release
        // count can be 3. That is a deferral, never a hazard —
        // Scintilla keeps the document alive until the map re-points,
        // which `refresh_tab_chrome` does unconditionally after every
        // drain / open / switch / close. A refactor that makes that
        // resync conditional would turn the deferral into a leak for
        // as long as the map sits on the dead tab's document.
        self.editor.send(SCI_RELEASEDOCUMENT, 0, doc);
    }

    fn convert_doc_eols(&mut self, doc: isize, eol: Eol) -> bool {
        self.with_doc(
            doc,
            |ui| {
                let mode = sc_eol_for(eol);
                // Mode first, so a document with nothing to convert
                // still ends up inserting the requested ending. The
                // conversion is one undo group and leaves the save
                // point alone (`Document::ConvertLineEnds`), which is
                // what the trait requires — see `replace_doc_text` for
                // why a `set_buffer_text`-style reinstall would be wrong.
                //
                // The `SCN_MODIFIED`s it emits re-enter `on_sci_notify`
                // under the dispatch borrow and are declined; the
                // plugin bridge's `update_status` covers the status bar
                // and `plugin::dispatch_nppm` re-polls the dirty marker
                // once the borrow is gone.
                ui.editor.send(SCI_SETEOLMODE, mode, 0);
                ui.editor.send(SCI_CONVERTEOLS, mode, 0);
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

    fn set_clipboard(&mut self, payloads: &[ClipboardData]) -> bool {
        set_clipboard_payloads(payloads)
    }

    fn register_dock_dialog(
        &mut self,
        params: codepp_plugin_host::DockDialogParams,
    ) -> Option<codepp_core::dock::DockPanel> {
        // `hClient` is an `NSView*` on this backend; the checks and the
        // adoption live beside the plugin bridge. See
        // `crate::plugin::register_dock_dialog`.
        crate::plugin::register_dock_dialog(params)
    }

    fn record_panel_open_command(
        &mut self,
        panel: codepp_core::dock::DockPanel,
        command: i32,
        seal: Option<codepp_core::dock::CommandSeal>,
    ) {
        crate::dock::set_open_command(panel, command, seal);
    }

    fn show_dock_dialog(&mut self, h_client: codepp_plugin_host::Hwnd) -> bool {
        crate::dock::show_plugin_panel(h_client)
    }

    fn hide_dock_dialog(&mut self, h_client: codepp_plugin_host::Hwnd) -> bool {
        crate::dock::hide_plugin_panel(h_client)
    }

    fn view_other_dock_tab(&mut self, name: &str) -> bool {
        crate::dock::view_plugin_panel(name)
    }

    fn update_dock_disp_info(&mut self, h_client: codepp_plugin_host::Hwnd) -> bool {
        crate::plugin::update_dock_disp_info(h_client)
    }

    fn dock_hwnd_by_name(&self, name: &str, module_name: Option<&str>) -> codepp_plugin_host::Hwnd {
        crate::dock::plugin_panel_handle(name, module_name).unwrap_or(std::ptr::null_mut())
    }

    fn set_npp_menu_item_check(&mut self, idm: i32, checked: bool) -> bool {
        // A plugin's own commands only: the built-in `IDM_*` ids are not
        // mapped on this backend, as `NPPM_MENUCOMMAND` is not, and the
        // View menu's own marks come from live state in
        // `validateMenuItem:` on every open anyway. Unlike the dock
        // overrides above, this one touches AppKit from inside the
        // dispatch's borrow — the item's state, if the menu is open, and
        // the command's toolbar button, if it has one — which is safe
        // because setting either state calls back into nothing of ours.
        // See `crate::plugin::set_menu_check`.
        let known = crate::plugin::set_menu_check(&self.menu, idm, checked);
        if known {
            self.toolbar.set_plugin_button_state(idm, checked);
        }
        known
    }

    fn register_modeless_dialog(&mut self, dlg: codepp_plugin_host::Hwnd, register: bool) -> bool {
        // `dlg` is an `NSWindow*` on this backend, and registering changes
        // nothing; see `crate::plugin::register_modeless_dialog`.
        crate::plugin::register_modeless_dialog(dlg, register, &self.window)
    }

    fn add_toolbar_icon(&mut self, cmd_id: i32, hicon: codepp_plugin_host::Hwnd) -> bool {
        // `hicon` is an `NSImage*` on this backend. Adding the button
        // touches AppKit from inside the dispatch's borrow, which is safe
        // for the reason `set_npp_menu_item_check` gives.
        crate::plugin::add_toolbar_icon(&self.toolbar, cmd_id, hicon)
    }

    fn create_plugin_scintilla(
        &mut self,
        parent: codepp_plugin_host::Hwnd,
    ) -> codepp_plugin_host::Hwnd {
        // `parent` is an `NSView*`, or the npp handle for a view in no
        // window; see `crate::plugin::create_plugin_scintilla`.
        crate::plugin::create_plugin_scintilla(parent, &self.window)
    }
}

/// Place the abstract clipboard `payloads` on the general pasteboard.
///
/// Drives `CODEPPM_SETCLIPBOARD`, which `cppexport`'s "Copy … to
/// Clipboard" items use. Self-contained (touches no `CocoaUi` state), so
/// it can be unit-adjacent and called from anywhere on the main thread.
///
/// **One `clearContents` for the whole set, then one `setData:forType:`
/// per format.** That is what makes a multi-format copy a single
/// clipboard *offer* rather than three that overwrite each other —
/// pasting into a rich-text target gets the RTF, into a browser the
/// HTML, into a terminal the plain text, all from one copy.
///
/// The plain-text fallback is guaranteed one layer up, in
/// `HostBridge::set_clipboard`, which synthesises a `Plain` payload when
/// a plugin sends only a rich one. This function therefore just maps
/// what it is handed, exactly like `ui_gtk::set_clipboard_payloads`.
pub(crate) fn set_clipboard_payloads(payloads: &[ClipboardData]) -> bool {
    if payloads.is_empty() {
        return false;
    }
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    let mut wrote_any = false;
    for payload in payloads {
        let (ty, bytes) = match payload {
            ClipboardData::Plain(b) => (unsafe { NSPasteboardTypeString }, b),
            ClipboardData::Html(b) => (unsafe { NSPasteboardTypeHTML }, b),
            ClipboardData::Rtf(b) => (unsafe { NSPasteboardTypeRTF }, b),
        };
        let data = NSData::with_bytes(bytes);
        // `setData:forType:` returns NO if the type was not declared by
        // the preceding `clearContents`/`addTypes:` cycle — but on the
        // general pasteboard a bare `setData:forType:` after
        // `clearContents` declares as it goes, which is the documented
        // modern usage.
        wrote_any |= pasteboard.setData_forType(Some(&data), ty);
    }
    wrote_any
}
