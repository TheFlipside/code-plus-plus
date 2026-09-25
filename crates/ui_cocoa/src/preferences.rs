//! The Preferences dialog for the Cocoa backend.
//!
//! Mirrors `ui_gtk::preferences` and the Win32 `preferences` module, and
//! edits the same two categories: **Recent Files History**
//! ([`codepp_core::preferences::RecentFilesHistoryConfig`]), which shapes
//! the File menu's recent-files region, and **Security**
//! ([`codepp_core::preferences::SecurityConfig`]), the guard on plugin
//! panels' startup commands. On Close the controls are read back and
//! written through `Shell::set_preferences`, which clamps and persists
//! them; the File menu picks the change up the next time it opens, since
//! that region is rebuilt on every open, and the guard is consulted at
//! the next start.
//!
//! **Why tabs in an `NSAlert` rather than a window.** Win32 and GTK show
//! their categories as a list beside the page it picks. With two
//! categories and a handful of controls each, a tab view is the Mac shape
//! of the same thing — the one `System Settings`-era apps still use for
//! a small pane — and it keeps the dialog an alert with an accessory
//! view, a modal path this backend already exercises in three places,
//! where a fresh `NSWindow` would need its own `setReleasedWhenClosed(false)`
//! lifecycle care (DESIGN.md §7.4 records why). Both pages are read back
//! at Close, the one not showing included.
//!
//! It lives in the **application menu** as "Settings…" with ⌘, — the
//! placement and shortcut macOS mandates — rather than under a Settings
//! menu, per the m1 convention decision.

use objc2::rc::Retained;
use objc2::{sel, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSBox, NSButton, NSButtonType, NSFont, NSTabView, NSTextField, NSTextFieldBezelStyle,
    NSView,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use codepp_core::preferences::{
    Preferences, RecentFileDisplayMode, RecentFilesHistoryConfig, CUSTOM_MAX_LENGTH_LIMIT,
    MAX_ENTRIES_LIMIT,
};

use crate::menu::Actions;
use crate::state::with_state;

/// Page geometry. The Recent Files History rows are laid out top-down
/// from the page's real height, one row at a time, so they cannot
/// collide — the same discipline m4a's Find panel had to be corrected to
/// after its Replace field overlapped its buttons. `HISTORY_HEIGHT` is
/// what those rows need; the pages are made at least that tall.
const WIDTH: f64 = 400.0;
const HISTORY_HEIGHT: f64 = 210.0;
const ROW: f64 = 24.0;
const GAP: f64 = 4.0;
const INDENT: f64 = 12.0;
/// What the tab view's own chrome takes around a page: the tab row and
/// the page inset, measured at 46 pt on macOS 26 for the Find panel's
/// tab view and rounded up. Only a floor for the tab view's size — every
/// page is laid out from the tab view's live `contentRect`.
const TAB_CHROME: f64 = 50.0;
/// Padding inside the Security page's "Plugin panels" box.
const BOX_PAD: f64 = 10.0;
/// What a titled `NSBox` takes around its content — the title row and the
/// borders: measured at 24 pt on macOS 26, plus 4 pt of margin, the same
/// margin [`TAB_CHROME`] carries.
const BOX_CHROME: f64 = 28.0;
/// How much narrower than [`WIDTH`] the help text is measured before the
/// page exists, to size the page: more than the tab view's and the box's
/// insets together, so the real width is never narrower than the one the
/// height was taken at — and the text, re-fitted to it, only gets shorter.
const HELP_WIDTH_ALLOWANCE: f64 = 64.0;

/// The Security page's checkbox: what the switch does, in the Win32
/// pane's words.
const VERIFY_PANEL_COMMANDS_LABEL: &str = "Run only signed plugin panel commands at startup";

/// What the checkbox means, below it — the Win32 pane's text, with how
/// this platform keeps the key in place of DPAPI (an owner-only file, as
/// on Linux; see `codepp_platform::panel_key`), the same words `ui_gtk`
/// uses.
const VERIFY_PANEL_COMMANDS_HELP: &str = "\
Code++ reopens a plugin's panel at startup the way Notepad++ does: \
by running the plugin's own command for it. It signs each command it \
records from the panel's own plugin, with a key kept in your Code++ \
settings folder that no other account can read.\n\n\
With this on, Code++ neither runs a command it did not sign nor loads \
its plugin at startup. That covers a command from an edited or copied \
session file, and one set by a plugin that is not the panel's own. \
The panel keeps its place until you open it from its plugin's menu.\n\n\
With this off, every saved command runs, as in Notepad++.";

/// The controls the read-back needs, both pages'. Held only for the
/// life of one modal session.
pub(crate) struct Controls {
    history: HistoryControls,
    /// The Security page's one switch.
    verify_panel_commands: Retained<NSButton>,
}

/// The Recent Files History page's controls.
struct HistoryControls {
    dont_check: Retained<NSButton>,
    in_submenu: Retained<NSButton>,
    max_entries: Retained<NSTextField>,
    custom_length: Retained<NSTextField>,
    only_name: Retained<NSButton>,
    custom: Retained<NSButton>,
}

/// Show the modal Preferences dialog, then persist any change.
pub(crate) fn show() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(current) = with_state(|st| st.shell.preferences.clone()) else {
        return;
    };

    // A nested modal session services the GCD main-queue source, so it
    // takes the same freeze every other modal on this backend does — a
    // worker wake could otherwise drain the shell mid-dialog.
    let _freeze = crate::DrainFreeze::new();
    let (alert, controls) = build_dialog(&current, mtm);
    alert.runModal();

    let mut updated = current.clone();
    updated.recent_files_history = read_back(&controls.history, &current.recent_files_history);
    updated.security.verify_panel_commands = controls.verify_panel_commands.state() != 0;
    if updated != current {
        with_state(|st| st.shell.set_preferences(updated));
    }
    // The recent-files region is rebuilt on the next File-menu open, so
    // nothing else has to be told. Matches `ui_gtk`.
}

/// Build the dialog and its controls.
///
/// Split from [`show`] so the wiring can be checked without entering a
/// modal session — `runModal` cannot return without a human. Same
/// reasoning, and the same split, as `search::build_goto_alert`.
pub(crate) fn build_dialog(
    prefs: &Preferences,
    mtm: MainThreadMarker,
) -> (Retained<NSAlert>, Controls) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Preferences"));
    alert.addButtonWithTitle(&NSString::from_str("Close"));

    // The Security page's help text decides its height, so it is measured
    // before the tab view is sized: both pages get the taller of the two.
    let help = help_label(VERIFY_PANEL_COMMANDS_HELP, mtm);
    let required_security_height =
        security_page_height(fit_to_width(&help, WIDTH - HELP_WIDTH_ALLOWANCE));
    let tabs = NSTabView::initWithFrame(
        NSTabView::alloc(mtm),
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(
                WIDTH,
                HISTORY_HEIGHT.max(required_security_height) + TAB_CHROME,
            ),
        ),
    );

    let (history_page, history_height) =
        crate::search::tab_page("Recent Files History", &tabs, mtm);
    let history = build_history_page(
        &history_page,
        history_height,
        &prefs.recent_files_history,
        mtm,
    );
    let (security_page, security_height) = crate::search::tab_page("Security", &tabs, mtm);
    let verify_panel_commands = build_security_page(
        &security_page,
        security_height,
        &help,
        prefs.security.verify_panel_commands,
        mtm,
    );

    alert.setAccessoryView(Some(&tabs));
    (
        alert,
        Controls {
            history,
            verify_panel_commands,
        },
    )
}

/// The Recent Files History page's rows, laid out top-down in `page`,
/// whose height is `height`.
fn build_history_page(
    page: &NSView,
    height: f64,
    cfg: &RecentFilesHistoryConfig,
    mtm: MainThreadMarker,
) -> HistoryControls {
    let width = page.frame().size.width;
    // Laid out top-down in *description*, bottom-up in coordinates:
    // `y` walks downward from the top edge, one row at a time.
    let mut y = height - ROW;

    // Negative-sense checkbox: checked means the feature is OFF
    // (`enabled == false`), matching N++'s and Win32's "Don't check at
    // launch time" wording. `read_back` inverts it symmetrically.
    let dont_check = check_box("Don't check at launch time", 0.0, y, width, mtm);
    dont_check.setState(isize::from(!cfg.enabled));
    page.addSubview(&dont_check);
    y -= ROW + GAP;

    let max_label = label(
        &format!("Max. number of entries (0 - {MAX_ENTRIES_LIMIT}):"),
        INDENT,
        y,
        240.0,
        mtm,
    );
    page.addSubview(&max_label);
    let max_entries = number_field(cfg.max_entries, INDENT + 248.0, y, mtm);
    page.addSubview(&max_entries);
    y -= ROW + GAP * 3.0;

    let display_label = label("Display:", 0.0, y, width, mtm);
    page.addSubview(&display_label);
    y -= ROW;

    let in_submenu = check_box("In Submenu", INDENT, y, width - INDENT, mtm);
    in_submenu.setState(isize::from(cfg.in_submenu));
    page.addSubview(&in_submenu);
    y -= ROW;

    // The three modes are radio buttons, and all three carry
    // [`Actions::codeppPrefsDisplayMode`] purely so AppKit groups them:
    // the click needs no handling, since the states are read back at
    // Close.
    //
    // **The shared action is load-bearing, not decoration**, and it was
    // measured rather than assumed after a reviewer read it as dead
    // wiring — a plausible reading, since `radio` sets no target, and
    // AppKit's headers document the grouping mechanism nowhere. Three
    // radio buttons in one superview, one pre-selected, then a click on
    // another: with the action they end `[0, 0, 1]`, without it
    // `[1, 0, 1]`. So a shared superview alone does *not* group them,
    // and dropping the action would let the dialog hold two modes at
    // once — which `read_back` would then resolve by precedence rather
    // than by what the user sees selected.
    let only_name = radio("Only File Name", INDENT, y, mtm);
    page.addSubview(&only_name);
    y -= ROW;
    let full_path = radio("Full File Name Path", INDENT, y, mtm);
    page.addSubview(&full_path);
    y -= ROW;
    let custom = radio("Customize Maximum Length:", INDENT, y, mtm);
    page.addSubview(&custom);
    let custom_length = number_field(cfg.custom_max_length, INDENT + 248.0, y, mtm);
    page.addSubview(&custom_length);
    y -= ROW;
    let hint = label(
        &format!("(1 - {CUSTOM_MAX_LENGTH_LIMIT})"),
        INDENT + 248.0,
        y,
        160.0,
        mtm,
    );
    page.addSubview(&hint);

    match cfg.display_mode {
        RecentFileDisplayMode::OnlyFileName => only_name.setState(1),
        RecentFileDisplayMode::FullPath => full_path.setState(1),
        RecentFileDisplayMode::CustomMaxLength => custom.setState(1),
    }

    HistoryControls {
        dont_check,
        in_submenu,
        max_entries,
        custom_length,
        only_name,
        custom,
    }
}

/// The Security page: one "Plugin panels" box holding the switch and what
/// it means — the Win32 pane's frame and wording. Returns the switch.
fn build_security_page(
    page: &NSView,
    height: f64,
    help: &NSTextField,
    verify: bool,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let width = page.frame().size.width;
    let frame = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
    );
    frame.setTitle(&NSString::from_str("Plugin panels"));
    page.addSubview(&frame);
    // A titled box always has a content view; the box itself is the
    // fallback only so there is no panic path in a dialog.
    let inside: Retained<NSView> = frame
        .contentView()
        .unwrap_or_else(|| Retained::into_super(frame.clone()));
    let inner = inside.frame().size;
    let help_height = fit_to_width(help, inner.width - 2.0 * BOX_PAD);
    let (switch_y, help_y) = security_rows(inner.height, help_height);
    let switch = check_box(
        VERIFY_PANEL_COMMANDS_LABEL,
        BOX_PAD,
        switch_y,
        inner.width - 2.0 * BOX_PAD,
        mtm,
    );
    switch.setState(isize::from(verify));
    inside.addSubview(&switch);
    help.setFrameOrigin(NSPoint::new(BOX_PAD, help_y));
    inside.addSubview(help);
    switch
}

/// Where the switch and the help text sit in the box's content view,
/// `inner_height` tall: the switch a padding below the top, the text a
/// gap below the switch. Each is the bottom edge, in the content view's
/// unflipped coordinates. A box too short for the text puts it at the
/// bottom rather than below it — [`security_page_height`] sizes the page
/// so that does not happen.
fn security_rows(inner_height: f64, help_height: f64) -> (f64, f64) {
    let switch_y = inner_height - BOX_PAD - ROW;
    (switch_y, (switch_y - GAP - help_height).max(0.0))
}

/// A wrapping label of `text` in the small system font.
fn help_label(text: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let help = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    help.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    help
}

/// Wrap `label` at `width` and make it as tall as its text then needs;
/// returns that height.
fn fit_to_width(label: &NSTextField, width: f64) -> f64 {
    label.setPreferredMaxLayoutWidth(width);
    let height = label.fittingSize().height.ceil();
    label.setFrameSize(NSSize::new(width, height));
    height
}

/// The Security page's height for help text `help_height` tall: the
/// box's title and borders, its padding, the switch, a gap, the text.
fn security_page_height(help_height: f64) -> f64 {
    BOX_CHROME + 2.0 * BOX_PAD + ROW + GAP + help_height
}

/// Read the controls back into a config.
///
/// `previous` supplies any field the dialog does not edit, so a pane
/// that grows later cannot silently reset what it does not show.
/// Out-of-range and unparseable numbers fall back to the previous value
/// rather than to a constant: a user who typed nonsense meant to change
/// nothing, and `Shell::set_preferences` clamps again regardless.
fn read_back(
    controls: &HistoryControls,
    previous: &RecentFilesHistoryConfig,
) -> RecentFilesHistoryConfig {
    let mut out = previous.clone();
    out.enabled = controls.dont_check.state() == 0;
    out.in_submenu = controls.in_submenu.state() != 0;
    out.max_entries = parse_bounded(
        &controls.max_entries.stringValue().to_string(),
        0,
        MAX_ENTRIES_LIMIT,
    )
    .unwrap_or(previous.max_entries);
    out.custom_max_length = parse_bounded(
        &controls.custom_length.stringValue().to_string(),
        1,
        CUSTOM_MAX_LENGTH_LIMIT,
    )
    .unwrap_or(previous.custom_max_length);
    out.display_mode = display_mode_of(
        controls.only_name.state() != 0,
        controls.custom.state() != 0,
    );
    out
}

/// Which display mode two of the three radio states describe.
///
/// Pure, and tested, because it encodes the one thing about this dialog
/// that can be silently wrong: the third state is *implied*, so a
/// mis-ordered check reports Full Path for a Customize selection and the
/// user's max-length value is quietly ignored.
fn display_mode_of(only_name: bool, custom: bool) -> RecentFileDisplayMode {
    if only_name {
        RecentFileDisplayMode::OnlyFileName
    } else if custom {
        RecentFileDisplayMode::CustomMaxLength
    } else {
        RecentFileDisplayMode::FullPath
    }
}

/// Parse a decimal in `min..=max`, or `None` for anything else —
/// including a value out of range, which is not the same as a value to
/// be clamped: clamping a typo silently applies a number the user never
/// chose.
fn parse_bounded(text: &str, min: u32, max: u32) -> Option<u32> {
    let value: u32 = text.trim().parse().ok()?;
    (min..=max).contains(&value).then_some(value)
}

// --- small control constructors ---------------------------------------

fn label(text: &str, x: f64, y: f64, width: f64, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFrame(NSRect::new(
        NSPoint::new(x, y),
        NSSize::new(width, ROW - GAP),
    ));
    field
}

fn check_box(title: &str, x: f64, y: f64, width: f64, mtm: MainThreadMarker) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, ROW - GAP)),
    );
    button.setButtonType(NSButtonType::Switch);
    button.setTitle(&NSString::from_str(title));
    button
}

fn radio(title: &str, x: f64, y: f64, mtm: MainThreadMarker) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect::new(NSPoint::new(x, y), NSSize::new(236.0, ROW - GAP)),
    );
    button.setButtonType(NSButtonType::Radio);
    button.setTitle(&NSString::from_str(title));
    // Grouping only — see the call site for why this is required
    // rather than decorative.
    //
    // SAFETY: a compile-time `sel!` literal that `Actions` implements.
    // No target is set, so nothing is dispatched; AppKit reads the
    // selector to decide which buttons form one exclusive group.
    unsafe {
        button.setAction(Some(sel!(codeppPrefsDisplayMode:)));
    }
    button
}

fn number_field(value: u32, x: f64, y: f64, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::initWithFrame(
        NSTextField::alloc(mtm),
        NSRect::new(NSPoint::new(x, y), NSSize::new(80.0, ROW - GAP)),
    );
    field.setStringValue(&NSString::from_str(&value.to_string()));
    field.setBezelStyle(NSTextFieldBezelStyle::RoundedBezel);
    field
}

/// Referenced so the grouping selector cannot be renamed out from under
/// [`radio`] without a compile error.
const _: fn(MainThreadMarker) -> Retained<Actions> = Actions::new;

#[cfg(test)]
mod tests {
    use super::*;

    /// A page as tall as [`security_page_height`] asks for keeps the help
    /// text clear of the switch and inside the box's padding, for any
    /// height of text rather than only today's — so the two functions
    /// cannot drift apart. What this cannot check is AppKit's own chrome;
    /// the box's is [`BOX_CHROME`], and the running dialog's frames were
    /// read back in-process: the switch and the text inside the box, apart.
    #[test]
    fn the_security_rows_fit_the_page_they_size() {
        for help_height in [0.0, 14.0, 42.0, 120.0] {
            let inner = security_page_height(help_height) - BOX_CHROME;
            let (switch_y, help_y) = security_rows(inner, help_height);
            assert!(
                help_y >= BOX_PAD,
                "{help_height} pt of text runs into the box's bottom padding"
            );
            assert!(
                help_y + help_height + GAP <= switch_y + 1e-9,
                "{help_height} pt of text runs into the switch"
            );
            assert!(
                switch_y + ROW + BOX_PAD <= inner + 1e-9,
                "the switch runs into the box's title"
            );
        }
    }

    #[test]
    fn display_mode_reads_the_implied_third_state() {
        assert_eq!(
            display_mode_of(true, false),
            RecentFileDisplayMode::OnlyFileName
        );
        assert_eq!(
            display_mode_of(false, true),
            RecentFileDisplayMode::CustomMaxLength
        );
        // Neither set is Full Path — the implied one.
        assert_eq!(
            display_mode_of(false, false),
            RecentFileDisplayMode::FullPath
        );
        // Both set cannot happen through the radio group, but if it ever
        // did, "Only File Name" wins rather than the answer depending on
        // evaluation order somewhere else.
        assert_eq!(
            display_mode_of(true, true),
            RecentFileDisplayMode::OnlyFileName
        );
    }

    #[test]
    fn out_of_range_and_nonsense_are_rejected_not_clamped() {
        assert_eq!(parse_bounded("5", 0, 30), Some(5));
        assert_eq!(parse_bounded("  7  ", 0, 30), Some(7));
        assert_eq!(parse_bounded("0", 0, 30), Some(0));
        assert_eq!(parse_bounded("30", 0, 30), Some(30));
        // Past the cap: rejected, so the caller keeps the old value.
        assert_eq!(parse_bounded("31", 0, 30), None);
        // Below the floor.
        assert_eq!(parse_bounded("0", 1, 40), None);
        // Not a number at all.
        assert_eq!(parse_bounded("", 0, 30), None);
        assert_eq!(parse_bounded("twelve", 0, 30), None);
        assert_eq!(parse_bounded("-1", 0, 30), None);
        assert_eq!(parse_bounded("3.5", 0, 30), None);
    }
}
