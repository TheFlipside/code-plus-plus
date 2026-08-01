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
    SCI_ADDUNDOACTION, SCI_BEGINUNDOACTION, SCI_COLOURISE, SCI_CREATEDOCUMENT, SCI_EMPTYUNDOBUFFER,
    SCI_ENDUNDOACTION, SCI_GETANCHOR, SCI_GETCOLUMN, SCI_GETCURRENTPOS, SCI_GETDOCPOINTER,
    SCI_GETFIRSTVISIBLELINE, SCI_GETLENGTH, SCI_GETLINECOUNT, SCI_GETMODIFY, SCI_GETOVERTYPE,
    SCI_GETSELECTIONEND, SCI_GETSELECTIONSTART, SCI_GETTEXT, SCI_GETXOFFSET, SCI_GETZOOM,
    SCI_GOTOPOS, SCI_LINEFROMPOSITION, SCI_LINESCROLL, SCI_LINESONSCREEN, SCI_POSITIONAFTER,
    SCI_SETDOCPOINTER, SCI_SETEMPTYSELECTION, SCI_SETEOLMODE, SCI_SETSAVEPOINT, SCI_SETSEL,
    SCI_SETSELECTIONEND, SCI_SETSELECTIONSTART, SCI_SETTABWIDTH, SCI_SETTEXT, SCI_SETXOFFSET,
    SCI_STYLEGETBACK, SCI_STYLEGETFORE, SC_EOL_CR, SC_EOL_CRLF, SC_EOL_LF, STYLE_DEFAULT,
};
use codepp_shell::{SearchFlags, UiPlatform};
use objc2_foundation::{NSPoint, NSRect, NSSize};

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
        let tabs_h = if self.tabs.is_hidden() {
            0.0
        } else {
            crate::tabs::TAB_STRIP_HEIGHT
        };
        // Zero while the dock is closed, which is most of the time, and
        // otherwise clamped to what this window size can give it — see
        // `FifDock::height_for_layout`.
        let dock_h = self
            .fif_dock
            .height_for_layout(size.height, tabs_h, toolbar_h);
        // Bottom-up, in Cocoa's unflipped content coordinates: status bar,
        // results dock, editor, tab strip, toolbar. The editor absorbs
        // what is left, and is floored at zero so a very short window
        // cannot ask for a negative height.
        let status_h = crate::status::STATUS_BAR_HEIGHT;
        let editor_h = (size.height - status_h - dock_h - tabs_h - toolbar_h).max(0.0);
        // Zero while the map is closed, and otherwise clamped to what
        // this window width can give it — see `DocMapPanel::width_for_layout`.
        // Unlike the bottom dock this one splits the band *horizontally*,
        // so it comes out of the editor's width rather than its height.
        let map_w = crate::docmap::width_for_layout(size.width);
        let editor_w = (size.width - map_w).max(0.0);
        self.status.container.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(size.width, status_h),
        ));
        self.fif_dock.container.setFrame(NSRect::new(
            NSPoint::new(0.0, status_h),
            NSSize::new(size.width, dock_h),
        ));
        self.sci_view.setFrame(NSRect::new(
            NSPoint::new(0.0, status_h + dock_h),
            NSSize::new(editor_w, editor_h),
        ));
        // The map shares the editor's band, pinned to the right edge. The
        // status bar, the results dock and both top strips stay full
        // width — the same arrangement Notepad++ and `ui_gtk` use, where
        // the map splits only the editing area.
        //
        // **Only while it is open.** Giving a closed panel a zero-width
        // frame reads as harmless — it is hidden — and is not: its
        // subviews are width-sizable, so they collapse to zero too, and
        // autoresizing cannot restore proportions from a zero-width
        // superview. Measured before this guard: reopening the map came
        // back with its header label 160 pt wide inside a 160 pt panel,
        // running straight over the close button. A hidden view's frame
        // is unobservable, so leaving it untouched costs nothing.
        if map_w > 0.0 {
            self.docmap.container.setFrame(NSRect::new(
                NSPoint::new(editor_w, status_h + dock_h),
                NSSize::new(map_w, editor_h),
            ));
        }
        self.tabs.container.setFrame(NSRect::new(
            NSPoint::new(0.0, status_h + dock_h + editor_h),
            NSSize::new(size.width, tabs_h),
        ));
        self.toolbar.container.setFrame(NSRect::new(
            NSPoint::new(0.0, status_h + dock_h + editor_h + tabs_h),
            NSSize::new(size.width, toolbar_h),
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
/// resets, then fix up the line-number margin for this backend.
///
/// The shared helper sets margin 0 to `SC_MARGIN_TEXT`, because
/// `ui_win32` renders the digits itself to get them right-aligned —
/// which means the host must write per-line margin text and keep it in
/// step with every edit. That machinery is Win32-private, so a Cocoa
/// buffer using `SC_MARGIN_TEXT` would show an empty gutter. Override to
/// Scintilla's built-in `SC_MARGIN_NUMBER`, which formats and paints the
/// numbers with no host involvement. The difference is alignment only,
/// and it is visible line numbers versus none. Exactly what GTK does,
/// and its comment already anticipated this backend.
pub(crate) fn apply_predefined_styles(editor: &EditorHandle) {
    codepp_editor::theme::apply_line_number_margin(editor);
    editor.enable_line_number_margin(LINE_NUMBER_MARGIN);
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
        let mode = match eol {
            Eol::CrLf => SC_EOL_CRLF,
            Eol::Cr => SC_EOL_CR,
            // `Mixed` has no Scintilla equivalent; LF is the least
            // surprising choice for new lines and matches the others.
            Eol::Lf | Eol::Mixed => SC_EOL_LF,
        };
        self.editor.send(SCI_SETEOLMODE, mode, 0);
        // UDL label resolution needs the registry, which m2 does not
        // reach yet (the UDL container-lexer path is a later milestone —
        // see `apply_lang`). Built-in languages resolve here; a UDL
        // buffer would read as its fallback name until then.
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
        // The shared Lexilla theme table, exactly as GTK uses it. The UDL
        // container-lexer branch GTK has in front of this is not ported
        // yet — `crate::udl` is a later milestone — so a UDL id falls
        // through to the table and renders unstyled rather than wrongly.
        codepp_editor::theme::apply_lang_theme(&self.editor, lang);
        // `apply_lang_theme` routes through `apply_default_styles`, which
        // resets margin 0 to `SC_MARGIN_TEXT` for Win32's manual
        // renderer. Re-assert the built-in number margin, or a file load
        // or language change would blank the gutter.
        self.editor.enable_line_number_margin(LINE_NUMBER_MARGIN);
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
        // Cocoa equivalent and needs no compositor cooperation.
        let transparency = styles.effective_transparency();
        self.window.setAlphaValue(if transparency.enabled {
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

    fn mark_active_buffer_dirty(&mut self) {
        if self.editor.send(SCI_GETMODIFY, 0, 0) != 0 {
            return;
        }
        // An empty undo action is the documented way to move a buffer
        // off its save point without changing a byte of text.
        self.editor.send(SCI_ADDUNDOACTION, 0, 0);
    }
}
