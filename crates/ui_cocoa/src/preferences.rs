//! The Preferences dialog for the Cocoa backend.
//!
//! Mirrors `ui_gtk::preferences` and the Win32 `preferences` module. It
//! edits the one pane wired on any backend so far — **Recent Files
//! History** ([`codepp_core::preferences::RecentFilesHistoryConfig`]),
//! which shapes the File menu's recent-files region. On Close the
//! controls are read back and written through `Shell::set_preferences`,
//! which clamps and persists them; the File menu picks the change up the
//! next time it opens, since that region is rebuilt on every open.
//!
//! **Why an `NSAlert` rather than a window.** The Win32 dialog is a
//! category-list + panel design; with a single category that is a lot of
//! chrome around four controls, and `ui_gtk` reaches the same conclusion
//! with a plain framed modal. An alert with an accessory view is the
//! Cocoa shape of the same thing, and it reuses a modal path this
//! backend already exercises in three places — where a fresh `NSWindow`
//! would need its own `setReleasedWhenClosed(false)` lifecycle care
//! (DESIGN.md §7.4 records why).
//!
//! It lives in the **application menu** as "Settings…" with ⌘, — the
//! placement and shortcut macOS mandates — rather than under a Settings
//! menu, per the m1 convention decision.

use objc2::rc::Retained;
use objc2::{sel, MainThreadOnly};
use objc2_app_kit::{NSAlert, NSButton, NSButtonType, NSTextField, NSTextFieldBezelStyle, NSView};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use codepp_core::preferences::{
    RecentFileDisplayMode, RecentFilesHistoryConfig, CUSTOM_MAX_LENGTH_LIMIT, MAX_ENTRIES_LIMIT,
};

use crate::menu::Actions;
use crate::state::with_state;

/// Accessory-view geometry. Laid out bottom-up from a fixed height, so
/// the rows cannot collide — the same discipline m4a's Find panel had to
/// be corrected to after its Replace field overlapped its buttons.
const WIDTH: f64 = 400.0;
const HEIGHT: f64 = 210.0;
const ROW: f64 = 24.0;
const GAP: f64 = 4.0;
const INDENT: f64 = 12.0;

/// The controls the read-back needs. Held only for the life of one
/// modal session.
pub(crate) struct Controls {
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
    let (alert, controls) = build_dialog(&current.recent_files_history, mtm);
    alert.runModal();

    let mut updated = current.clone();
    updated.recent_files_history = read_back(&controls, &current.recent_files_history);
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
    cfg: &RecentFilesHistoryConfig,
    mtm: MainThreadMarker,
) -> (Retained<NSAlert>, Controls) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Preferences"));
    alert.setInformativeText(&NSString::from_str("Recent Files History"));
    alert.addButtonWithTitle(&NSString::from_str("Close"));

    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)),
    );

    // Laid out top-down in *description*, bottom-up in coordinates:
    // `y` walks downward from the top edge, one row at a time.
    let mut y = HEIGHT - ROW;

    // Negative-sense checkbox: checked means the feature is OFF
    // (`enabled == false`), matching N++'s and Win32's "Don't check at
    // launch time" wording. `read_back` inverts it symmetrically.
    let dont_check = check_box("Don't check at launch time", 0.0, y, WIDTH, mtm);
    dont_check.setState(isize::from(!cfg.enabled));
    view.addSubview(&dont_check);
    y -= ROW + GAP;

    let max_label = label(
        &format!("Max. number of entries (0 - {MAX_ENTRIES_LIMIT}):"),
        INDENT,
        y,
        240.0,
        mtm,
    );
    view.addSubview(&max_label);
    let max_entries = number_field(cfg.max_entries, INDENT + 248.0, y, mtm);
    view.addSubview(&max_entries);
    y -= ROW + GAP * 3.0;

    let display_label = label("Display:", 0.0, y, WIDTH, mtm);
    view.addSubview(&display_label);
    y -= ROW;

    let in_submenu = check_box("In Submenu", INDENT, y, WIDTH - INDENT, mtm);
    in_submenu.setState(isize::from(cfg.in_submenu));
    view.addSubview(&in_submenu);
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
    view.addSubview(&only_name);
    y -= ROW;
    let full_path = radio("Full File Name Path", INDENT, y, mtm);
    view.addSubview(&full_path);
    y -= ROW;
    let custom = radio("Customize Maximum Length:", INDENT, y, mtm);
    view.addSubview(&custom);
    let custom_length = number_field(cfg.custom_max_length, INDENT + 248.0, y, mtm);
    view.addSubview(&custom_length);
    y -= ROW;
    let hint = label(
        &format!("(1 - {CUSTOM_MAX_LENGTH_LIMIT})"),
        INDENT + 248.0,
        y,
        160.0,
        mtm,
    );
    view.addSubview(&hint);

    match cfg.display_mode {
        RecentFileDisplayMode::OnlyFileName => only_name.setState(1),
        RecentFileDisplayMode::FullPath => full_path.setState(1),
        RecentFileDisplayMode::CustomMaxLength => custom.setState(1),
    }

    alert.setAccessoryView(Some(&view));
    (
        alert,
        Controls {
            dont_check,
            in_submenu,
            max_entries,
            custom_length,
            only_name,
            custom,
        },
    )
}

/// Read the controls back into a config.
///
/// `previous` supplies any field the dialog does not edit, so a pane
/// that grows later cannot silently reset what it does not show.
/// Out-of-range and unparseable numbers fall back to the previous value
/// rather than to a constant: a user who typed nonsense meant to change
/// nothing, and `Shell::set_preferences` clamps again regardless.
fn read_back(controls: &Controls, previous: &RecentFilesHistoryConfig) -> RecentFilesHistoryConfig {
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
