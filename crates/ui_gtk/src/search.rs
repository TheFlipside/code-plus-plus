//! Find/Replace and Goto.
//!
//! The search *logic* is not here — it lives in `Shell::{find_next,
//! find_prev, replace_current, replace_all, count_matches}` and the
//! `UiPlatform::search_*` methods, all cross-platform and shared with
//! `ui_win32`. This module is only the GTK chrome that drives them: two
//! dialogs and the glue that reaches `Shell` through the thread-local
//! state the rest of the backend uses.
//!
//! # Goto is modal, Find/Replace is modeless
//!
//! Goto asks one question and acts once, so a modal `gtk::Dialog` that
//! blocks until the user answers is the honest shape. Find/Replace has
//! to stay open while the user reads results and edits the buffer
//! between clicks — that is what "modeless" means and what Notepad++
//! does — so it is a `gtk::Window` the user can leave open, held on
//! [`crate::state::GtkUiState`] for the session.
//!
//! # Re-entrancy
//!
//! A modeless dialog's button handler runs on the main thread and
//! reaches `Shell` through [`with_state`], exactly like a menu handler.
//! `with_state` already refuses a re-entrant borrow, so a handler that
//! fires while another is mid-flight is skipped rather than aliasing
//! `&mut Shell` — the same guarantee every other handler relies on.

use gtk::glib;
use gtk::prelude::*;

use codepp_shell::SearchFlags;

use crate::state::with_state;

/// Read the three option checkboxes into the `SearchFlags` the shared
/// drivers expect. One place, so Find and Replace cannot disagree about
/// what "match case" means.
fn flags_from(match_case: bool, whole_word: bool, regex: bool) -> SearchFlags {
    let mut f = SearchFlags::NONE;
    if match_case {
        f = f.union(SearchFlags::MATCH_CASE);
    }
    if whole_word {
        f = f.union(SearchFlags::WHOLE_WORD);
    }
    if regex {
        f = f.union(SearchFlags::REGEX);
    }
    f
}

/// Show the modal Goto dialog and jump to the line the user enters.
///
/// One-based, matching the status bar and every editor convention;
/// Scintilla's `SCI_GOTOLINE` is zero-based, so the value is decremented
/// on the way in. An out-of-range or unparseable value does nothing
/// rather than clamping silently to the last line, which would move the
/// caret somewhere the user did not ask for.
pub fn show_goto() {
    let Some(parent) = with_state(|st| st.window.clone()) else {
        return;
    };
    let dialog = gtk::Dialog::with_buttons(
        Some("Go to line"),
        Some(&parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[
            ("_Cancel", gtk::ResponseType::Cancel),
            ("_Go", gtk::ResponseType::Accept),
        ],
    );
    dialog.set_default_response(gtk::ResponseType::Accept);

    let content = dialog.content_area();
    content.set_spacing(6);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(8);
    content.set_margin_end(8);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.pack_start(&gtk::Label::new(Some("Line:")), false, false, 0);
    let entry = gtk::Entry::new();
    entry.set_input_purpose(gtk::InputPurpose::Digits);
    // Enter in the field triggers the default (Go) rather than needing
    // a separate click.
    entry.set_activates_default(true);
    row.pack_start(&entry, true, true, 0);
    content.pack_start(&row, false, false, 0);
    dialog.show_all();

    if dialog.run() == gtk::ResponseType::Accept {
        if let Ok(line) = entry.text().trim().parse::<usize>() {
            if line >= 1 {
                with_state(|st| {
                    // Zero-based for Scintilla; `line - 1` cannot
                    // underflow given the `>= 1` guard above.
                    st.editor
                        .send(codepp_scintilla_sys::SCI_GOTOLINE, line - 1, 0);
                });
            }
        }
    }
    // SAFETY: created here, never handed out; `destroy` on gtk-rs is
    // `unsafe` only because destroying a widget others hold invalidates
    // it.
    unsafe {
        dialog.destroy();
    }
}

/// The modeless Find/Replace dialog's widgets, held on the window state
/// for the session.
///
/// One dialog serves both Find and Replace: opening Replace reveals the
/// replace row that Find hides, matching Notepad++'s single dialog with
/// a mode toggle rather than two separate windows.
pub struct FindReplaceDialog {
    window: gtk::Window,
    find_entry: gtk::Entry,
    replace_entry: gtk::Entry,
    replace_row: gtk::Box,
    replace_buttons: gtk::Box,
    match_case: gtk::CheckButton,
    whole_word: gtk::CheckButton,
    regex: gtk::CheckButton,
    /// Transient one-line result readout ("3 replaced", "not found").
    status: gtk::Label,
}

impl FindReplaceDialog {
    /// Read the current option checkboxes.
    fn flags(&self) -> SearchFlags {
        flags_from(
            self.match_case.is_active(),
            self.whole_word.is_active(),
            self.regex.is_active(),
        )
    }
}

/// Open Find, building the dialog on first use.
pub fn show_find() {
    open_dialog(false);
}

/// Open Replace, building the dialog on first use and revealing the
/// replace controls.
pub fn show_replace() {
    open_dialog(true);
}

/// Ensure the dialog exists, set its mode, and present it.
///
/// Focus goes to the find field with any current selection prefilled,
/// which is what a user pressing Ctrl+F over a highlighted word
/// expects. Reusing the one dialog rather than building a second is the
/// whole reason it lives on the state.
fn open_dialog(replace_mode: bool) {
    // Build outside `with_state` if needed: constructing the dialog does
    // not touch `Shell`, and doing it inside would hold the borrow
    // across widget setup for no reason.
    let exists = with_state(|st| st.find_replace.is_some()).unwrap_or(false);
    if !exists {
        let dialog = build_dialog();
        with_state(|st| st.find_replace = Some(dialog));
    }

    // Prefill from the current selection, so Ctrl+F on a word searches
    // for it. Empty selection leaves whatever was there before.
    let selection = with_state(|st| {
        let start = st
            .editor
            .send(codepp_scintilla_sys::SCI_GETSELECTIONSTART, 0, 0);
        let end = st
            .editor
            .send(codepp_scintilla_sys::SCI_GETSELECTIONEND, 0, 0);
        if end > start {
            Some(read_selection(&st.editor))
        } else {
            None
        }
    })
    .flatten();

    with_state(|st| {
        let Some(d) = &st.find_replace else {
            return;
        };
        set_replace_visible(d, replace_mode);
        if let Some(text) = &selection {
            if !text.contains('\n') {
                d.find_entry.set_text(text);
            }
        }
        d.status.set_text("");
        d.window.show_all();
        set_replace_visible(d, replace_mode);
        d.window.present();
        d.find_entry.grab_focus();
    });
}

/// Show or hide the replace row and its buttons.
fn set_replace_visible(d: &FindReplaceDialog, visible: bool) {
    d.replace_row.set_visible(visible);
    d.replace_buttons.set_visible(visible);
    let title = if visible { "Replace" } else { "Find" };
    d.window.set_title(title);
}

/// Read the active editor's current selection as a `String`.
fn read_selection(editor: &codepp_editor::EditorHandle) -> String {
    let len = editor
        .send(codepp_scintilla_sys::SCI_GETSELTEXT, 0, 0)
        .max(0) as usize;
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u8; len + 1];
    editor.send(
        codepp_scintilla_sys::SCI_GETSELTEXT,
        0,
        buf.as_mut_ptr() as isize,
    );
    buf.truncate(len);
    String::from_utf8_lossy(&buf).into_owned()
}

/// F3 / the dialog's Find Next: repeat the last search forward.
pub fn find_next_repeat() {
    let found = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.find_next_repeat(&mut ui)
    })
    .flatten();
    report_find(found);
}

/// Shift+F3 / Find Previous: repeat the last search backward.
pub fn find_prev_repeat() {
    let found = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.find_prev_repeat(&mut ui)
    })
    .flatten();
    report_find(found);
}

/// Put a not-found note on the dialog status line if it is open. A
/// found match moves the caret, which is feedback enough.
fn report_find(found: Option<u64>) {
    if found.is_some() {
        return;
    }
    with_state(|st| {
        if let Some(d) = &st.find_replace {
            d.status.set_text("Not found");
        }
    });
}

/// Build the dialog and wire every button. The widgets it returns are
/// stored on the state; the handlers reach `Shell` through `with_state`
/// when they fire, so they capture no state themselves.
fn build_dialog() -> FindReplaceDialog {
    let parent = with_state(|st| st.window.clone());
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Find");
    window.set_transient_for(parent.as_ref());
    window.set_type_hint(gtk::gdk::WindowTypeHint::Dialog);
    window.set_resizable(false);
    // Modeless: closing it must hide, not destroy, so the next Ctrl+F
    // reuses it. Destroying would dangle the state's reference.
    window.connect_delete_event(|w, _| {
        w.hide();
        glib::Propagation::Stop
    });

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.set_margin_top(10);
    outer.set_margin_bottom(10);
    outer.set_margin_start(10);
    outer.set_margin_end(10);
    window.add(&outer);

    // Find row.
    let find_entry = gtk::Entry::new();
    find_entry.set_activates_default(false);
    let find_row = labelled_row("Find:", &find_entry);
    outer.pack_start(&find_row, false, false, 0);

    // Replace row (hidden in Find mode).
    let replace_entry = gtk::Entry::new();
    let replace_row = labelled_row("Replace:", &replace_entry);
    outer.pack_start(&replace_row, false, false, 0);

    // Options.
    let match_case = gtk::CheckButton::with_label("Match case");
    let whole_word = gtk::CheckButton::with_label("Whole word");
    let regex = gtk::CheckButton::with_label("Regular expression");
    let opts = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    opts.pack_start(&match_case, false, false, 0);
    opts.pack_start(&whole_word, false, false, 0);
    opts.pack_start(&regex, false, false, 0);
    outer.pack_start(&opts, false, false, 0);

    // Find buttons.
    let find_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_next = gtk::Button::with_label("Find Next");
    let btn_prev = gtk::Button::with_label("Find Previous");
    let btn_count = gtk::Button::with_label("Count");
    find_buttons.pack_start(&btn_next, false, false, 0);
    find_buttons.pack_start(&btn_prev, false, false, 0);
    find_buttons.pack_start(&btn_count, false, false, 0);
    outer.pack_start(&find_buttons, false, false, 0);

    // Replace buttons (hidden in Find mode).
    let replace_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_replace = gtk::Button::with_label("Replace");
    let btn_replace_all = gtk::Button::with_label("Replace All");
    replace_buttons.pack_start(&btn_replace, false, false, 0);
    replace_buttons.pack_start(&btn_replace_all, false, false, 0);
    outer.pack_start(&replace_buttons, false, false, 0);

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    outer.pack_start(&status, false, false, 0);

    // Handlers reach the dialog's own widgets back through `with_state`
    // when they fire, so they can read the query and options that are
    // live at click time rather than a snapshot from build time.
    btn_next.connect_clicked(|_| do_find(true));
    btn_prev.connect_clicked(|_| do_find(false));
    btn_count.connect_clicked(|_| do_count());
    btn_replace.connect_clicked(|_| do_replace_one());
    btn_replace_all.connect_clicked(|_| do_replace_all());
    // Enter in the find field is Find Next.
    find_entry.connect_activate(|_| do_find(true));

    FindReplaceDialog {
        window,
        find_entry,
        replace_entry,
        replace_row,
        replace_buttons,
        match_case,
        whole_word,
        regex,
        status,
    }
}

/// A `Label: [entry]` row with the label right-aligned to a fixed width
/// so Find and Replace fields line up.
fn labelled_row(text: &str, entry: &gtk::Entry) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let label = gtk::Label::new(Some(text));
    label.set_xalign(1.0);
    label.set_width_chars(8);
    row.pack_start(&label, false, false, 0);
    entry.set_width_chars(28);
    row.pack_start(entry, true, true, 0);
    row
}

/// Read the query and flags out of the open dialog, then run one Find.
///
/// Returns early if the dialog is gone or the query is empty. The empty
/// case is not an error — it is the state the field starts in.
fn do_find(forward: bool) {
    let params = with_state(|st| {
        st.find_replace
            .as_ref()
            .map(|d| (d.find_entry.text().to_string(), d.flags()))
    })
    .flatten();
    let Some((query, flags)) = params else {
        return;
    };
    if query.is_empty() {
        return;
    }
    let found = with_state(|st| {
        let (shell, mut ui) = st.split();
        if forward {
            shell.find_next(&mut ui, &query, flags)
        } else {
            shell.find_prev(&mut ui, &query, flags)
        }
    })
    .flatten();
    with_state(|st| {
        if let Some(d) = &st.find_replace {
            d.status
                .set_text(if found.is_some() { "" } else { "Not found" });
        }
    });
}

/// Count matches without moving the caret or the selection.
fn do_count() {
    let params = with_state(|st| {
        st.find_replace
            .as_ref()
            .map(|d| (d.find_entry.text().to_string(), d.flags()))
    })
    .flatten();
    let Some((query, flags)) = params else {
        return;
    };
    if query.is_empty() {
        return;
    }
    let count = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.count_matches(&mut ui, &query, flags)
    })
    .unwrap_or(0);
    with_state(|st| {
        if let Some(d) = &st.find_replace {
            d.status.set_text(&format!("{count} matches"));
        }
    });
}

/// Replace the current selection if it matches, then advance.
fn do_replace_one() {
    let params = with_state(|st| {
        st.find_replace.as_ref().map(|d| {
            (
                d.find_entry.text().to_string(),
                d.replace_entry.text().to_string(),
                d.flags(),
            )
        })
    })
    .flatten();
    let Some((query, replacement, flags)) = params else {
        return;
    };
    if query.is_empty() {
        return;
    }
    let replaced = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.replace_current(&mut ui, &query, &replacement, flags)
    })
    .unwrap_or(false);
    with_state(|st| {
        if let Some(d) = &st.find_replace {
            d.status
                .set_text(if replaced { "Replaced" } else { "Not found" });
        }
    });
}

/// Replace every match in the buffer as one undoable action.
fn do_replace_all() {
    let params = with_state(|st| {
        st.find_replace.as_ref().map(|d| {
            (
                d.find_entry.text().to_string(),
                d.replace_entry.text().to_string(),
                d.flags(),
            )
        })
    })
    .flatten();
    let Some((query, replacement, flags)) = params else {
        return;
    };
    if query.is_empty() {
        return;
    }
    let n = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.replace_all(&mut ui, &query, &replacement, flags)
    })
    .unwrap_or(0);
    with_state(|st| {
        if let Some(d) = &st.find_replace {
            d.status.set_text(&format!("{n} replaced"));
        }
    });
}
