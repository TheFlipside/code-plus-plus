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

/// Notebook page index for each mode. Matches `ui_win32`'s
/// Find/Replace/Find-in-Files tab order.
const PAGE_FIND: u32 = 0;
const PAGE_REPLACE: u32 = 1;
const PAGE_FIND_IN_FILES: u32 = 2;

/// The modeless Find/Replace/Find-in-Files dialog's widgets, held on the
/// window state for the session.
///
/// One window serves all three modes as notebook tabs, matching
/// Notepad++ (and `ui_win32`): the query field and the Match case / Whole
/// word / Regular expression options sit above the notebook and are
/// *shared* across every tab, while each tab carries only its own
/// controls — Find has just its buttons, Replace adds a replacement
/// field, and Find in Files adds directory / filters / its own
/// replacement plus the two scan-scope checkboxes.
pub struct FindReplaceDialog {
    window: gtk::Window,
    notebook: gtk::Notebook,
    find_entry: gtk::Entry,
    replace_entry: gtk::Entry,
    match_case: gtk::CheckButton,
    whole_word: gtk::CheckButton,
    regex: gtk::CheckButton,
    /// Find-in-Files tab: directory to search.
    fif_directory: gtk::Entry,
    /// Find-in-Files tab: whitespace-separated include globs.
    fif_filters: gtk::Entry,
    /// Find-in-Files tab: its own replacement field (Replace in Files).
    fif_replace: gtk::Entry,
    /// Find-in-Files tab: recurse into subdirectories.
    fif_subfolders: gtk::CheckButton,
    /// Find-in-Files tab: descend into hidden (dot-prefixed) folders.
    fif_hidden: gtk::CheckButton,
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

/// Open the Find tab, building the dialog on first use.
pub fn show_find() {
    open_dialog(PAGE_FIND);
}

/// Open the Replace tab, building the dialog on first use.
pub fn show_replace() {
    open_dialog(PAGE_REPLACE);
}

/// Open the Find-in-Files tab, building the dialog on first use.
pub fn show_find_in_files() {
    open_dialog(PAGE_FIND_IN_FILES);
}

/// Ensure the dialog exists, select `page`, and present it.
///
/// Focus goes to the find field with any current selection prefilled,
/// which is what a user pressing Ctrl+F over a highlighted word
/// expects. Reusing the one dialog rather than building a second is the
/// whole reason it lives on the state.
fn open_dialog(page: u32) {
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

    // Seed the Find-in-Files directory with the active file's folder on
    // first open, matching Notepad++ — only when empty, so a user's own
    // entry is never clobbered.
    let seed_dir = if page == PAGE_FIND_IN_FILES {
        with_state(|st| {
            st.shell
                .active_tab
                .and_then(|i| st.shell.tabs.get(i))
                .and_then(|t| t.path.as_ref())
                .and_then(|p| p.parent())
                .map(|p| p.to_string_lossy().into_owned())
        })
        .flatten()
    } else {
        None
    };

    with_state(|st| {
        let Some(d) = &st.find_replace else {
            return;
        };
        set_page(d, page);
        if let Some(text) = &selection {
            if !text.contains('\n') {
                d.find_entry.set_text(text);
            }
        }
        if let Some(dir) = &seed_dir {
            // Only auto-seed a display-clean path. The field is functional
            // — it becomes the search root — so it can't be sanitized in
            // place without corrupting the path; a component carrying
            // control / bidi characters would otherwise render them into
            // the Entry. When it isn't clean, leave the field for the user.
            if d.fif_directory.text().is_empty()
                && codepp_shell::sanitize_str_for_display(dir) == *dir
            {
                d.fif_directory.set_text(dir);
            }
        }
        d.status.set_text("");
        d.window.show_all();
        set_page(d, page);
        d.window.present();
        if page == PAGE_FIND_IN_FILES && d.find_entry.text().is_empty() {
            d.fif_directory.grab_focus();
        } else {
            d.find_entry.grab_focus();
        }
    });
}

/// Select the notebook page and retitle the window to match.
fn set_page(d: &FindReplaceDialog, page: u32) {
    d.notebook.set_current_page(Some(page));
    let title = match page {
        PAGE_REPLACE => "Replace",
        PAGE_FIND_IN_FILES => "Find in Files",
        _ => "Find",
    };
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
    // Escape closes (hides) the window too, matching the modal dialogs
    // (Goto, the confirm prompts) that get it from GTK for free. A plain
    // `gtk::Window` has no such behaviour, so wire it explicitly; hide
    // rather than destroy for the same reuse reason as delete-event.
    window.connect_key_press_event(|w, ev| {
        if ev.keyval() == gtk::gdk::keys::constants::Escape {
            w.hide();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.set_margin_top(10);
    outer.set_margin_bottom(10);
    outer.set_margin_start(10);
    outer.set_margin_end(10);
    window.add(&outer);

    // --- Shared: query field + options (above the notebook) --------
    let find_entry = gtk::Entry::new();
    find_entry.set_activates_default(false);
    outer.pack_start(&labelled_row("Find:", &find_entry), false, false, 0);

    let match_case = gtk::CheckButton::with_label("Match case");
    let whole_word = gtk::CheckButton::with_label("Whole word");
    let regex = gtk::CheckButton::with_label("Regular expression");
    let opts = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    opts.pack_start(&match_case, false, false, 0);
    opts.pack_start(&whole_word, false, false, 0);
    opts.pack_start(&regex, false, false, 0);
    outer.pack_start(&opts, false, false, 0);

    let notebook = gtk::Notebook::new();
    outer.pack_start(&notebook, false, false, 0);

    // --- Find page -------------------------------------------------
    let find_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    find_page.set_margin_top(8);
    let find_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_next = gtk::Button::with_label("Find Next");
    let btn_prev = gtk::Button::with_label("Find Previous");
    let btn_count = gtk::Button::with_label("Count");
    find_buttons.pack_start(&btn_next, false, false, 0);
    find_buttons.pack_start(&btn_prev, false, false, 0);
    find_buttons.pack_start(&btn_count, false, false, 0);
    find_page.pack_start(&find_buttons, false, false, 0);
    notebook.append_page(&find_page, Some(&gtk::Label::new(Some("Find"))));

    // --- Replace page ----------------------------------------------
    let replace_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    replace_page.set_margin_top(8);
    let replace_entry = gtk::Entry::new();
    replace_page.pack_start(&labelled_row("Replace:", &replace_entry), false, false, 0);
    let replace_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_replace = gtk::Button::with_label("Replace");
    let btn_replace_all = gtk::Button::with_label("Replace All");
    replace_buttons.pack_start(&btn_replace, false, false, 0);
    replace_buttons.pack_start(&btn_replace_all, false, false, 0);
    replace_page.pack_start(&replace_buttons, false, false, 0);
    notebook.append_page(&replace_page, Some(&gtk::Label::new(Some("Replace"))));

    // --- Find in Files page ----------------------------------------
    let fif = build_fif_page(&notebook);

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
        notebook,
        find_entry,
        replace_entry,
        match_case,
        whole_word,
        regex,
        fif_directory: fif.directory,
        fif_filters: fif.filters,
        fif_replace: fif.replace,
        fif_subfolders: fif.subfolders,
        fif_hidden: fif.hidden,
        status,
    }
}

/// The Find-in-Files tab's own widgets, returned by [`build_fif_page`].
struct FifPageWidgets {
    directory: gtk::Entry,
    filters: gtk::Entry,
    replace: gtk::Entry,
    subfolders: gtk::CheckButton,
    hidden: gtk::CheckButton,
}

/// Build the Find-in-Files notebook page, wire its buttons, append it to
/// `notebook`, and return its input widgets for the dialog struct.
fn build_fif_page(notebook: &gtk::Notebook) -> FifPageWidgets {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page.set_margin_top(8);

    let directory = gtk::Entry::new();
    let dir_row = labelled_row("Directory:", &directory);
    let btn_browse = gtk::Button::with_label("Browse…");
    dir_row.pack_start(&btn_browse, false, false, 0);
    page.pack_start(&dir_row, false, false, 0);

    let filters = gtk::Entry::new();
    filters.set_placeholder_text(Some("e.g. *.rs *.toml — empty for all files"));
    page.pack_start(&labelled_row("Filters:", &filters), false, false, 0);

    let replace = gtk::Entry::new();
    page.pack_start(&labelled_row("Replace with:", &replace), false, false, 0);

    let subfolders = gtk::CheckButton::with_label("In sub-folders");
    subfolders.set_active(true);
    let hidden = gtk::CheckButton::with_label("In hidden folders");
    let opts = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    opts.pack_start(&subfolders, false, false, 0);
    opts.pack_start(&hidden, false, false, 0);
    page.pack_start(&opts, false, false, 0);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let btn_find_all = gtk::Button::with_label("Find All");
    let btn_replace_in_files = gtk::Button::with_label("Replace in Files");
    buttons.pack_start(&btn_find_all, false, false, 0);
    buttons.pack_start(&btn_replace_in_files, false, false, 0);
    page.pack_start(&buttons, false, false, 0);

    notebook.append_page(&page, Some(&gtk::Label::new(Some("Find in Files"))));

    btn_browse.connect_clicked(|_| browse_fif_directory());
    btn_find_all.connect_clicked(|_| do_find_all());
    btn_replace_in_files.connect_clicked(|_| do_replace_in_files());

    FifPageWidgets {
        directory,
        filters,
        replace,
        subfolders,
        hidden,
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

// --- Find in Files ----------------------------------------------------

/// Read the Find-in-Files tab into a [`crate::fif::FifInputs`], including
/// the shared query/options and the tab's own directory/filters/scope.
///
/// `is_replace` decides whether the replacement string (the FIF tab's own
/// "Replace with" field) is captured — so Find All sends `None` and
/// Replace in Files sends `Some(..)`.
fn gather_fif_inputs(is_replace: bool) -> Option<crate::fif::FifInputs> {
    with_state(|st| {
        st.find_replace.as_ref().map(|d| crate::fif::FifInputs {
            query: d.find_entry.text().to_string(),
            replacement: is_replace.then(|| d.fif_replace.text().to_string()),
            match_case: d.match_case.is_active(),
            whole_word: d.whole_word.is_active(),
            regex: d.regex.is_active(),
            directory: d.fif_directory.text().to_string(),
            filters: d.fif_filters.text().to_string(),
            recurse: d.fif_subfolders.is_active(),
            hidden: d.fif_hidden.is_active(),
        })
    })
    .flatten()
}

/// Put a message on the dialog's status line, if it is open.
///
/// Sanitized: `msg` can be a `FifError` display string that embeds the
/// user's directory path (e.g. `BadRoot`), and a hostile path component
/// would otherwise render its control / bidi characters straight into the
/// chrome. Same substitute-with-U+FFFD policy the rest of the feature
/// applies to result rows and the destructive-confirm prompt.
fn set_fif_status(msg: &str) {
    let clean = codepp_shell::sanitize_str_for_display(msg);
    with_state(|st| {
        if let Some(d) = &st.find_replace {
            d.status.set_text(&clean);
        }
    });
}

/// Hide the Find/Replace window and clear its status. Called after a FIF
/// job starts so the results dock is unobstructed (matches `ui_win32`,
/// which hides the dialog on FIF start).
fn hide_search_window() {
    with_state(|st| {
        if let Some(d) = &st.find_replace {
            d.status.set_text("");
            d.window.hide();
        }
    });
}

/// "Find All" on the Find-in-Files tab: start a plain search.
fn do_find_all() {
    let Some(inputs) = gather_fif_inputs(false) else {
        return;
    };
    match crate::fif::run_search(inputs) {
        Ok(()) => hide_search_window(),
        Err(msg) => set_fif_status(&msg),
    }
}

/// "Replace in Files": confirm the destructive rewrite, then start it.
///
/// All inputs are captured *before* the confirm modal runs, so the
/// wording the user approves cannot diverge from the parameters actually
/// executed — the modal spins a nested main loop that would otherwise let
/// the fields change between Yes and dispatch. Mirrors `ui_win32`.
fn do_replace_in_files() {
    let Some(inputs) = gather_fif_inputs(true) else {
        return;
    };
    // Validate here — duplicating `run_search`'s own empty-input guards —
    // specifically so an empty query/directory doesn't pop the destructive
    // confirm dialog before `run_search` would reject it anyway.
    if inputs.query.is_empty() {
        set_fif_status("Enter a search term");
        return;
    }
    if inputs.directory.trim().is_empty() {
        set_fif_status("Enter a directory to search");
        return;
    }
    // Sanitized: this prompt gates a destructive on-disk rewrite, and
    // `set_secondary_text` renders control characters, so scrub them from
    // the echoed query / replacement / path.
    let confirm = format!(
        "Replace \"{}\" with \"{}\" in files under {}{}?\n\nThis rewrites matching files on disk and cannot be undone.",
        codepp_shell::sanitize_str_for_display(&inputs.query),
        codepp_shell::sanitize_str_for_display(inputs.replacement.as_deref().unwrap_or("")),
        codepp_shell::sanitize_str_for_display(inputs.directory.trim()),
        if inputs.recurse { " (and sub-folders)" } else { "" },
    );
    let response = crate::message_dialog(
        gtk::MessageType::Warning,
        gtk::ButtonsType::YesNo,
        "Replace in Files",
        &confirm,
    );
    if response != gtk::ResponseType::Yes {
        return;
    }
    match crate::fif::run_search(inputs) {
        Ok(()) => hide_search_window(),
        Err(msg) => set_fif_status(&msg),
    }
}

/// Pick the Find-in-Files search directory with a native folder chooser
/// and write it into the Directory field.
fn browse_fif_directory() {
    let parent = with_state(|st| st.window.clone());
    let chooser = gtk::FileChooserNative::new(
        Some("Choose folder to search"),
        parent.as_ref(),
        gtk::FileChooserAction::SelectFolder,
        Some("_Select"),
        Some("_Cancel"),
    );
    let chosen = if chooser.run() == gtk::ResponseType::Accept {
        chooser.filename()
    } else {
        None
    };
    // `FileChooserNative` keeps its window alive until destroyed.
    chooser.destroy();
    if let Some(dir) = chosen {
        with_state(|st| {
            if let Some(d) = &st.find_replace {
                d.fif_directory.set_text(&dir.to_string_lossy());
            }
        });
    }
}
