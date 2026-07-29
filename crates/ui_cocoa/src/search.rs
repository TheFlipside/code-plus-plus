//! Find / Replace and Go To.
//!
//! # Two dialogs, two shapes, for a reason
//!
//! Go To is **modal**: it asks one question, acts once, and there is
//! nothing to keep on screen afterwards. Find/Replace is **modeless** —
//! the user searches, looks at the buffer, edits, searches again — which
//! is how Notepad++ and both other backends behave, and the reason it is
//! an `NSPanel` rather than a run-modal window. A modal Find would make
//! the editor untouchable between searches, which is precisely the
//! workflow it exists to support.
//!
//! `NSPanel` specifically, not a plain `NSWindow`: a panel does not
//! become the application's main window, so the editor keeps its
//! main-window status and its chrome behind the dialog. That is what
//! `NSWindowStyleMask::Utility` expresses, and it is the closest AppKit
//! has to the tool-window role `ui_win32`'s `WS_POPUP` class plays.
//!
//! # One window, three modes
//!
//! The query field and the Match case / Whole word / Regular expression
//! options live *above* an `NSTabView`, and are shared by every tab;
//! each tab carries only its own controls. Same split `ui_gtk` uses, so
//! the two dialogs read the same and a user's options survive switching
//! mode. The Find-in-Files tab arrives with the results dock — it needs
//! somewhere to put its output — so this is two tabs for now.
//!
//! An `NSTabView` here does **not** conflict with the single-Scintilla-view
//! rule that rules `NSTabView` out for the tab strip: the objection there
//! is that it owns per-tab content views, and a Scintilla view must not
//! be owned that way. These tabs own ordinary control panels.
//!
//! # Why the widgets live on the window state
//!
//! Every handler reaches the model through `with_state` when it fires
//! rather than capturing anything, so the panel's controls have to be
//! findable later. They are stored as `CocoaUiState.find_replace`, built
//! on first use and then reused — the same lifetime `ui_gtk` gives its
//! `FindReplaceDialog`.

use objc2::rc::Retained;
use objc2::{sel, AnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSBackingStoreType, NSButton, NSButtonType, NSPanel, NSTabView, NSTabViewItem,
    NSTextField, NSView, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use codepp_shell::SearchFlags;

use crate::menu::Actions;
use crate::state::with_state;

/// Tab indices, matching `ui_win32`'s and `ui_gtk`'s order.
const PAGE_FIND: isize = 0;
const PAGE_REPLACE: isize = 1;

/// Panel geometry. Fixed rather than auto-laid-out: the contents are a
/// handful of rows whose sizes never change, and springs-and-struts is
/// what the rest of this backend uses.
const PANEL_WIDTH: f64 = 480.0;
const PANEL_HEIGHT: f64 = 250.0;
const MARGIN: f64 = 14.0;
const ROW_HEIGHT: f64 = 22.0;
const LABEL_WIDTH: f64 = 92.0;
const BUTTON_WIDTH: f64 = 128.0;
const GAP: f64 = 8.0;

/// The modeless Find/Replace panel's controls, held on the window state
/// for the session. See the module docs.
pub struct FindReplaceDialog {
    pub panel: Retained<NSPanel>,
    tabs: Retained<NSTabView>,
    find_field: Retained<NSTextField>,
    replace_field: Retained<NSTextField>,
    match_case: Retained<NSButton>,
    whole_word: Retained<NSButton>,
    regex: Retained<NSButton>,
    /// One-line result readout ("3 replaced", "Not found").
    status: Retained<NSTextField>,
}

impl FindReplaceDialog {
    /// The option checkboxes as a [`SearchFlags`].
    fn flags(&self) -> SearchFlags {
        let mut f = SearchFlags::NONE;
        if self.match_case.state() != 0 {
            f = f.union(SearchFlags::MATCH_CASE);
        }
        if self.whole_word.state() != 0 {
            f = f.union(SearchFlags::WHOLE_WORD);
        }
        if self.regex.state() != 0 {
            f = f.union(SearchFlags::REGEX);
        }
        f
    }

    fn query(&self) -> String {
        self.find_field.stringValue().to_string()
    }

    fn replacement(&self) -> String {
        self.replace_field.stringValue().to_string()
    }

    fn set_status(&self, text: &str) {
        self.status.setStringValue(&NSString::from_str(text));
    }
}

/// Open the Find tab, building the panel on first use.
pub(crate) fn show_find() {
    open_panel(PAGE_FIND);
}

/// Open the Replace tab, building the panel on first use.
pub(crate) fn show_replace() {
    open_panel(PAGE_REPLACE);
}

/// Show the panel on `page`, seeding the query from the selection.
///
/// Seeding matches every editor's Find: whatever is selected is almost
/// always what the user wants to search for. Only a single-line selection
/// is used — a multi-line one is a block the user is working with, not a
/// search term, and pasting it into a one-line field would silently
/// truncate it.
///
/// **And only a display-clean one**, per [`seedable_query`].
fn open_panel(page: isize) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let exists = with_state(|st| st.find_replace.is_some()).unwrap_or(false);
    if !exists {
        let Some(dialog) = build_panel(mtm) else {
            return;
        };
        with_state(|st| st.find_replace = Some(dialog));
    }
    let selection = with_state(|st| read_selection(&st.editor)).unwrap_or_default();
    with_state(|st| {
        let Some(dialog) = &st.find_replace else {
            return;
        };
        if let Some(seed) = seedable_query(&selection) {
            dialog.find_field.setStringValue(&NSString::from_str(seed));
        }
        dialog.set_status("");
        dialog.tabs.selectTabViewItemAtIndex(page);
        // `makeKeyAndOrderFront:` rather than `orderFront:`: the query
        // field has to take the keystrokes that follow, and only a key
        // window's first responder does.
        dialog.panel.makeKeyAndOrderFront(None);
        dialog.panel.makeFirstResponder(Some(&dialog.find_field));
    });
}

/// Whether a selection may be auto-seeded into the query field, and the
/// text to use if so.
///
/// Three refusals, and the third is a security decision rather than a
/// usability one:
///
/// * Empty — nothing to seed.
/// * Multi-line — a block the user is working with, not a search term;
///   a one-line field would silently truncate it.
/// * **Not display-clean** — buffer text is attacker-influenced (a file
///   from disk or drag-and-drop, or a path a plugin opened), and a
///   selection carrying bidi overrides or zero-width characters would
///   render reordered or partly invisible in the field. DESIGN.md §7.4
///   catalogues three prior incidents of exactly this: hostile text
///   reaching chrome unsanitized.
///
/// The refusal is deliberately *not* a substitution, which is how every
/// other sink in this codebase handles it. A tab title or a status line
/// is a label, so replacing a hostile character with U+FFFD costs
/// nothing — but this field's contents are the actual search parameter,
/// and substituting there would quietly search for something the user
/// never asked for and report "not found" for text plainly on screen.
/// Leaving the field alone is the honest failure: the user can still
/// paste or type the term deliberately. Same call `ui_gtk` makes for its
/// Find-in-Files directory field.
fn seedable_query(selection: &str) -> Option<&str> {
    if selection.is_empty() || selection.contains('\n') {
        return None;
    }
    if codepp_shell::sanitize_str_for_display(selection) != selection {
        return None;
    }
    Some(selection)
}

/// Read the current selection as a `String`.
///
/// `SCI_GETSELTEXT` reports the length when handed a null pointer, then
/// fills a buffer of that size — the standard Scintilla two-call idiom.
fn read_selection(editor: &codepp_editor::EditorHandle) -> String {
    let len = editor
        .send(codepp_scintilla_sys::SCI_GETSELTEXT, 0, 0)
        .max(0) as usize;
    if len == 0 {
        return String::new();
    }
    // One extra byte: Scintilla writes a terminating NUL.
    let mut buf = vec![0u8; len + 1];
    editor.send(
        codepp_scintilla_sys::SCI_GETSELTEXT,
        0,
        buf.as_mut_ptr() as isize,
    );
    buf.truncate(len);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Read the query and flags out of the panel, if it is open and the
/// query is non-empty.
///
/// Every command below starts here, so "the panel is closed" and "the
/// field is blank" are handled once rather than five times.
fn query_and_flags() -> Option<(String, SearchFlags)> {
    let params =
        with_state(|st| st.find_replace.as_ref().map(|d| (d.query(), d.flags()))).flatten()?;
    if params.0.is_empty() {
        return None;
    }
    Some(params)
}

/// Re-sync the main window's chrome after a search command.
///
/// **Required, not defensive, for the replace commands.** Scintilla emits
/// `SCN_MODIFIED` (and `SCN_SAVEPOINTLEFT`) *synchronously* from inside
/// the edit, which means it re-enters `on_sci_notify` while the
/// `with_state` borrow that issued the replace is still held — and
/// `with_state` correctly declines a re-entrant call, so that
/// notification does nothing. `on_sci_notify`'s own documentation states
/// the resulting obligation: any path that changes buffer text from
/// inside a `with_state` closure must refresh the chrome itself
/// afterwards. Without this, "3 replaced" appears in the panel while the
/// tab's dirty marker and the status bar's length stay stale until some
/// unrelated event repaints them.
///
/// The find commands call it too. There it is genuinely defensive —
/// `SCN_UPDATEUI` arrives on Scintilla's next paint/idle pass, outside
/// any borrow of ours, so the status bar would catch up a moment later
/// anyway. Calling it removes the timing assumption rather than relying
/// on it.
fn refresh_chrome() {
    crate::refresh_tab_chrome();
    with_state(|st| {
        let (_, ui) = st.split();
        ui.refresh_dynamic_status();
    });
}

/// Write a line into the panel's status label, if it is open.
fn set_status(text: &str) {
    with_state(|st| {
        if let Some(dialog) = &st.find_replace {
            dialog.set_status(text);
        }
    });
}

/// Find Next / Find Previous, from the panel's buttons.
pub(crate) fn do_find(forward: bool) {
    let Some((query, flags)) = query_and_flags() else {
        return;
    };
    let found = with_state(|st| {
        let (shell, mut ui) = st.split();
        if forward {
            shell.find_next(&mut ui, &query, flags)
        } else {
            shell.find_prev(&mut ui, &query, flags)
        }
    })
    .flatten();
    refresh_chrome();
    set_status(if found.is_some() { "" } else { "Not found" });
}

/// Count matches without moving the caret or the selection.
pub(crate) fn do_count() {
    let Some((query, flags)) = query_and_flags() else {
        return;
    };
    let count = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.count_matches(&mut ui, &query, flags)
    })
    .unwrap_or(0);
    set_status(&format!("{count} matches"));
}

/// Replace the current selection if it matches, then advance.
pub(crate) fn do_replace_one() {
    let Some((query, flags)) = query_and_flags() else {
        return;
    };
    let replacement = with_state(|st| st.find_replace.as_ref().map(FindReplaceDialog::replacement))
        .flatten()
        .unwrap_or_default();
    let replaced = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.replace_current(&mut ui, &query, &replacement, flags)
    })
    .unwrap_or(false);
    refresh_chrome();
    set_status(if replaced { "Replaced" } else { "Not found" });
}

/// Replace every match in the buffer as one undoable action.
pub(crate) fn do_replace_all() {
    let Some((query, flags)) = query_and_flags() else {
        return;
    };
    let replacement = with_state(|st| st.find_replace.as_ref().map(FindReplaceDialog::replacement))
        .flatten()
        .unwrap_or_default();
    let n = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.replace_all(&mut ui, &query, &replacement, flags)
    })
    .unwrap_or(0);
    refresh_chrome();
    set_status(&format!("{n} replaced"));
}

/// F3 / Find Next from the menu: repeat the last search forward.
///
/// Distinct from [`do_find`]: it uses the *shell's* remembered query, so
/// it works with the panel closed — which is the point of a repeat.
pub(crate) fn find_next_repeat() {
    let found = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.find_next_repeat(&mut ui)
    })
    .flatten();
    report_repeat(found);
}

/// Shift+F3 / Find Previous: repeat the last search backward.
pub(crate) fn find_prev_repeat() {
    let found = with_state(|st| {
        let (shell, mut ui) = st.split();
        shell.find_prev_repeat(&mut ui)
    })
    .flatten();
    report_repeat(found);
}

/// Note a failed repeat on the panel's status line if it happens to be
/// open. A *found* match moves the caret, which is feedback enough, and
/// with the panel closed there is nowhere to put a message — the same
/// choice `ui_gtk` makes rather than interrupting with an alert.
fn report_repeat(found: Option<u64>) {
    refresh_chrome();
    if found.is_none() {
        set_status("Not found");
    }
}

/// Go To Line: a modal prompt that jumps the caret.
///
/// One-based, matching the status bar and every editor convention;
/// `SCI_GOTOLINE` is zero-based, so the value is decremented on the way
/// in. An out-of-range or unparseable value does nothing rather than
/// clamping to the last line, which would move the caret somewhere the
/// user did not ask for.
pub(crate) fn show_goto() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // The prompt runs a nested modal session, so the same freeze every
    // other modal on this backend takes applies: a worker wake dispatched
    // into it could otherwise move `active_tab` and the jump would land in
    // a different buffer than the one the user was looking at.
    let _freeze = crate::DrainFreeze::new();

    let (alert, field) = build_goto_alert(mtm);

    // `NSAlertFirstButtonReturn` is 1000 — the "Go" button.
    if alert.runModal() != 1000 {
        return;
    }
    let Some(zero_based) = parse_goto_line(&field.stringValue().to_string()) else {
        return;
    };
    with_state(|st| {
        st.editor
            .send(codepp_scintilla_sys::SCI_GOTOLINE, zero_based, 0);
    });
}

/// Build the Go To prompt: an alert with a text field in its accessory
/// slot.
///
/// Separated from [`show_goto`] so the construction can be checked
/// without entering a modal session — `runModal` cannot return without a
/// human, and it is machinery this backend already exercises in three
/// other places, so the part worth verifying is the wiring.
fn build_goto_alert(mtm: MainThreadMarker) -> (Retained<NSAlert>, Retained<NSTextField>) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Go to line"));
    alert.addButtonWithTitle(&NSString::from_str("Go"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    let field = NSTextField::initWithFrame(
        NSTextField::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(200.0, 24.0)),
    );
    alert.setAccessoryView(Some(&field));
    // Without this the field is not focused and the first keystroke lands
    // on the alert's buttons instead of in the box — so typing a line
    // number and pressing Return would do nothing.
    alert.window().setInitialFirstResponder(Some(&field));
    (alert, field)
}

/// Turn what the user typed into a zero-based Scintilla line, or `None`
/// if it is not a line number.
///
/// Split out so the parsing is testable without a modal: the interesting
/// cases are all input shapes rather than AppKit behaviour, and a modal
/// cannot be driven from `cargo test`.
///
/// `None` rather than a clamp for anything unusable. Clamping an
/// unparseable or zero input to line 1 would move the caret somewhere the
/// user never asked for, and a typo is far more likely than a deliberate
/// "take me to the top" via an empty box.
fn parse_goto_line(text: &str) -> Option<usize> {
    let line: usize = text.trim().parse().ok()?;
    // One-based in, zero-based out; `line >= 1` makes the subtraction safe.
    line.checked_sub(1)
}

/// Build the panel and wire every control.
///
/// Returns `None` only if the window state is unreachable, which means
/// startup has not finished — there is no panel to parent to yet.
fn build_panel(mtm: MainThreadMarker) -> Option<FindReplaceDialog> {
    let actions = with_state(|st| st.actions.clone())?;

    let frame = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(PANEL_WIDTH, PANEL_HEIGHT),
    );
    // `Utility` is what makes it a tool window rather than a document
    // window: it floats above the editor and does not become the
    // application's main window. Not resizable — every row is
    // fixed-width, so a resize would only add empty space.
    let style =
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::UtilityWindow;
    // `defer: false` creates the window-server backing immediately,
    // which is what an about-to-be-shown window wants.
    let panel: Retained<NSPanel> = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        style,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setTitle(&NSString::from_str("Find / Replace"));
    // Closing must hide, not destroy: the controls are held on the window
    // state and reused on the next ⌘F. Without this the panel would be
    // released out from under those references — the same
    // `releasedWhenClosed` trap m1 records for the main window, and it is
    // live here rather than latent because this window really is opened
    // and closed as ordinary control flow.
    // SAFETY: turning the release *off* can only make the window
    // outlive its close, never the reverse, so it cannot create a
    // dangling reference — which is the hazard this setter is `unsafe`
    // for.
    unsafe { panel.setReleasedWhenClosed(false) };
    // Escape closes it, as it does in Notepad++ and every macOS panel.
    panel.setHidesOnDeactivate(false);

    let content = panel.contentView()?;
    let mut y = PANEL_HEIGHT - MARGIN - ROW_HEIGHT - 22.0;

    // Shared: the query row.
    let find_field = labelled_row(&content, "Find what:", y, mtm);
    y -= ROW_HEIGHT + GAP;

    // Shared: the three option checkboxes, on one row.
    // Three columns sized from the panel rather than hard-coded, so the
    // last one cannot run past the right margin — an earlier version put
    // it 4 px over the edge and clipped the label.
    let options_x = MARGIN + LABEL_WIDTH;
    let column = (PANEL_WIDTH - MARGIN - options_x) / 3.0;
    let match_case = checkbox(&content, "Match case", options_x, y, column, mtm);
    let whole_word = checkbox(&content, "Whole word", options_x + column, y, column, mtm);
    let regex = checkbox(
        &content,
        "Regular expression",
        options_x + 2.0 * column,
        y,
        column,
        mtm,
    );
    y -= ROW_HEIGHT + GAP;

    // The tabs, filling what is left above the status line.
    let tabs_height = y - MARGIN - ROW_HEIGHT;
    let tabs = NSTabView::initWithFrame(
        NSTabView::alloc(mtm),
        NSRect::new(
            NSPoint::new(MARGIN, MARGIN + ROW_HEIGHT),
            NSSize::new(PANEL_WIDTH - 2.0 * MARGIN, tabs_height),
        ),
    );

    build_find_page(&tabs, &actions, mtm);
    let replace_field = build_replace_page(&tabs, &actions, mtm);
    content.addSubview(&tabs);

    // The status line, along the bottom.
    let status = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    status.setFrame(NSRect::new(
        NSPoint::new(MARGIN, MARGIN),
        NSSize::new(PANEL_WIDTH - 2.0 * MARGIN, ROW_HEIGHT),
    ));
    content.addSubview(&status);

    panel.center();

    Some(FindReplaceDialog {
        panel,
        tabs,
        find_field,
        replace_field,
        match_case,
        whole_word,
        regex,
        status,
    })
}

/// The Find tab: three buttons, no fields of its own — the query and
/// the options are shared and live above the tab view.
fn build_find_page(tabs: &NSTabView, actions: &Actions, mtm: MainThreadMarker) {
    let (page, _) = tab_page("Find", tabs, mtm);
    for (index, (title, action)) in [
        ("Find Next", sel!(codeppFindNext:)),
        ("Find Previous", sel!(codeppFindPrevious:)),
        ("Count", sel!(codeppCount:)),
    ]
    .into_iter()
    .enumerate()
    {
        #[allow(clippy::cast_precision_loss)]
        let x = index as f64 * (BUTTON_WIDTH + GAP);
        button(&page, title, x, 0.0, action, actions, mtm);
    }
}

/// The Replace tab: its own replacement field, plus two buttons. Returns
/// the field so the caller can keep it for later reads.
fn build_replace_page(
    tabs: &NSTabView,
    actions: &Actions,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let (page, height) = tab_page("Replace", tabs, mtm);
    let field = labelled_row_in(&page, "Replace with:", height - ROW_HEIGHT - GAP, mtm);
    for (index, (title, action)) in [
        ("Replace", sel!(codeppReplace:)),
        ("Replace All", sel!(codeppReplaceAll:)),
    ]
    .into_iter()
    .enumerate()
    {
        #[allow(clippy::cast_precision_loss)]
        let x = index as f64 * (BUTTON_WIDTH + GAP);
        button(&page, title, x, 0.0, action, actions, mtm);
    }
    field
}

/// Add an `NSTabViewItem` and return its content view plus that view's
/// height, which the caller needs to lay rows out from the top.
fn tab_page(label: &str, tabs: &NSTabView, mtm: MainThreadMarker) -> (Retained<NSView>, f64) {
    // SAFETY: `initWithIdentifier:` accepts nil for "no identifier",
    // which is what we want — the pages are addressed by index.
    let item: Retained<NSTabViewItem> =
        unsafe { NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), None) };
    item.setLabel(&NSString::from_str(label));
    // The tab view insets its pages; ask it for the resulting rect rather
    // than guessing at the inset, which differs between OS versions.
    let rect = tabs.contentRect();
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), rect.size),
    );
    item.setView(Some(&view));
    tabs.addTabViewItem(&item);
    (view, rect.size.height)
}

/// A label + text field row across the panel's full width.
fn labelled_row(
    content: &NSView,
    label: &str,
    y: f64,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    add_label(content, label, MARGIN, y, mtm);
    let field = NSTextField::initWithFrame(
        NSTextField::alloc(mtm),
        NSRect::new(
            NSPoint::new(MARGIN + LABEL_WIDTH, y),
            NSSize::new(PANEL_WIDTH - 2.0 * MARGIN - LABEL_WIDTH, ROW_HEIGHT),
        ),
    );
    content.addSubview(&field);
    field
}

/// The same row, inside a tab page (which is narrower than the panel).
fn labelled_row_in(
    page: &NSView,
    label: &str,
    y: f64,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    add_label(page, label, 0.0, y, mtm);
    let width = page.bounds().size.width - LABEL_WIDTH;
    let field = NSTextField::initWithFrame(
        NSTextField::alloc(mtm),
        NSRect::new(
            NSPoint::new(LABEL_WIDTH, y),
            NSSize::new(width.max(0.0), ROW_HEIGHT),
        ),
    );
    page.addSubview(&field);
    field
}

fn add_label(parent: &NSView, text: &str, x: f64, y: f64, mtm: MainThreadMarker) {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFrame(NSRect::new(
        NSPoint::new(x, y),
        NSSize::new(LABEL_WIDTH - GAP, ROW_HEIGHT),
    ));
    parent.addSubview(&label);
}

/// A checkbox. `NSButtonType::Switch` is AppKit's checkbox.
fn checkbox(
    content: &NSView,
    title: &str,
    x: f64,
    y: f64,
    width: f64,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, ROW_HEIGHT)),
    );
    button.setButtonType(NSButtonType::Switch);
    button.setTitle(&NSString::from_str(title));
    content.addSubview(&button);
    button
}

/// A push button in a tab page, positioned from the page's bottom-left.
fn button(
    page: &NSView,
    title: &str,
    x: f64,
    y: f64,
    action: objc2::runtime::Sel,
    actions: &Actions,
    mtm: MainThreadMarker,
) {
    let button = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect::new(NSPoint::new(x, y), NSSize::new(BUTTON_WIDTH, 26.0)),
    );
    button.setTitle(&NSString::from_str(title));
    // SAFETY: the selector is a compile-time `sel!` literal declared in
    // `Actions`'s own `define_class!` block, and the target is a weak
    // reference to an object the window state owns for the process
    // lifetime.
    unsafe {
        button.setTarget(Some(actions));
        button.setAction(Some(action));
    }
    page.addSubview(&button);
}

#[cfg(test)]
mod tests {
    use super::parse_goto_line;

    #[test]
    fn a_line_number_converts_to_zero_based() {
        assert_eq!(parse_goto_line("1"), Some(0));
        assert_eq!(parse_goto_line("12"), Some(11));
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(parse_goto_line("  7  "), Some(6));
        assert_eq!(parse_goto_line("\t3\n"), Some(2));
    }

    #[test]
    fn line_zero_is_refused_rather_than_clamped() {
        // One-based input, so 0 is not a line. Clamping it to the first
        // line would move the caret on what is almost certainly a typo.
        assert_eq!(parse_goto_line("0"), None);
    }

    #[test]
    fn anything_unparseable_is_refused() {
        for text in ["", "   ", "abc", "1.5", "-4", "1e3", "٣"] {
            assert_eq!(parse_goto_line(text), None, "{text:?} should not parse");
        }
    }
}

#[cfg(test)]
mod seed_tests {
    use super::seedable_query;

    #[test]
    fn an_ordinary_single_line_selection_seeds() {
        assert_eq!(seedable_query("needle"), Some("needle"));
        assert_eq!(seedable_query("  spaced  "), Some("  spaced  "));
    }

    #[test]
    fn nothing_and_multiline_do_not_seed() {
        assert_eq!(seedable_query(""), None);
        assert_eq!(seedable_query("two\nlines"), None);
        assert_eq!(seedable_query("trailing\n"), None);
    }

    #[test]
    fn display_hostile_text_does_not_seed() {
        // U+202E RIGHT-TO-LEFT OVERRIDE — the character behind the
        // "invoice\u{202E}fdp.exe" attack DESIGN.md §7.4 records. A
        // substitution would change the search term, so the answer is to
        // seed nothing.
        assert_eq!(seedable_query("invoice\u{202E}fdp.exe"), None);
        // Zero-width space, and a raw tab.
        assert_eq!(seedable_query("a\u{200B}b"), None);
        assert_eq!(seedable_query("a\tb"), None);
    }
}
