//! Notepad++'s menu command ids (its `menuCmdID.h`), the ones Code++
//! uses: the built-in commands `NPPM_MENUCOMMAND` and
//! `NPPM_SETMENUITEMCHECK` can name, which Win32 maps onto its own menu
//! (`ui_win32`'s `map_npp_idm_to_local`), the base the User Defined
//! Language rows count from (`NPPM_GETNBUSERLANG`), the
//! auto-completion commands, which Code++ does not map but which its
//! plugin commands must not be numbered on, and the two that bound
//! where Code++'s own Win32 commands may be numbered: Print Now and
//! the lowest of the other ids Notepad++ numbers below its menu
//! sections.
//!
//! A plugin sends these numbers compiled in from Notepad++'s header, so
//! each value must be Notepad++'s exactly, not just its name: a wrong
//! number is not a command that goes unanswered, it is a different
//! command answered confidently. `tools/npp-abi-check/check.py`
//! compares the constants here, and those in the compat header
//! `plugins/nppcompat-headers/menuCmdID.h`, with upstream's, and fails
//! on a name Notepad++ does not define or a declaration it cannot read.
//! Until Phase 5 nothing did, and eleven of the ids Win32 mapped were
//! wrong, some on other Notepad++ commands (DESIGN.md §7.4).

/// The base every built-in command counts from.
pub const IDM: i32 = 40_000;

/// The File menu.
pub const IDM_FILE: i32 = IDM + 1000;
/// File → New.
pub const IDM_FILE_NEW: i32 = IDM_FILE + 1;
/// File → Open.
pub const IDM_FILE_OPEN: i32 = IDM_FILE + 2;
/// File → Close.
pub const IDM_FILE_CLOSE: i32 = IDM_FILE + 3;
/// File → Close All.
pub const IDM_FILE_CLOSEALL: i32 = IDM_FILE + 4;
/// File → Save.
pub const IDM_FILE_SAVE: i32 = IDM_FILE + 6;
/// File → Save All.
pub const IDM_FILE_SAVEALL: i32 = IDM_FILE + 7;
/// File → Save As.
pub const IDM_FILE_SAVEAS: i32 = IDM_FILE + 8;
/// File → Print, with its dialog.
pub const IDM_FILE_PRINT: i32 = IDM_FILE + 10;
/// File → Exit.
pub const IDM_FILE_EXIT: i32 = IDM_FILE + 11;
/// File → Reload from Disk.
pub const IDM_FILE_RELOAD: i32 = IDM_FILE + 14;
/// File → Rename.
pub const IDM_FILE_RENAME: i32 = IDM_FILE + 17;
/// File → Print Now, which prints without the dialog.
///
/// Notepad++ numbers it below its menu sections, among the numbers
/// `ui_win32` gives its own commands, so Code++ must leave it free: a
/// plugin that sends it as a raw `WM_COMMAND`, a common way to drive
/// Notepad++, would otherwise run one of Code++'s commands. Until
/// Phase 5 it ran File → Exit (DESIGN.md §7.4).
pub const IDM_FILE_PRINTNOW: i32 = 1001;

/// The Edit menu.
pub const IDM_EDIT: i32 = IDM + 2000;
/// Edit → Cut.
pub const IDM_EDIT_CUT: i32 = IDM_EDIT + 1;
/// Edit → Copy.
pub const IDM_EDIT_COPY: i32 = IDM_EDIT + 2;
/// Edit → Undo.
pub const IDM_EDIT_UNDO: i32 = IDM_EDIT + 3;
/// Edit → Redo.
pub const IDM_EDIT_REDO: i32 = IDM_EDIT + 4;
/// Edit → Paste.
pub const IDM_EDIT_PASTE: i32 = IDM_EDIT + 5;
/// Edit → Delete.
pub const IDM_EDIT_DELETE: i32 = IDM_EDIT + 6;
/// Edit → Select All.
pub const IDM_EDIT_SELECTALL: i32 = IDM_EDIT + 7;

/// Edit → Auto-Completion → function completion. Notepad++ numbers its
/// six auto-completion commands above its menu sections, from 50000,
/// where Code++ numbered its plugin commands until Phase 5; Code++ maps
/// none of them, and the plugin command range now stays clear of all
/// six ([`crate::is_plugin_cmd_id`]).
pub const IDM_EDIT_AUTOCOMPLETE: i32 = 50_000;
/// Edit → Auto-Completion → word completion.
pub const IDM_EDIT_AUTOCOMPLETE_CURRENTFILE: i32 = 50_001;
/// Edit → Auto-Completion → the function-parameter hint.
pub const IDM_EDIT_FUNCCALLTIP: i32 = 50_002;
/// Edit → Auto-Completion → path completion.
pub const IDM_EDIT_AUTOCOMPLETE_PATH: i32 = 50_006;
/// Edit → Auto-Completion → the previous function-parameter hint.
pub const IDM_EDIT_FUNCCALLTIP_PREVIOUS: i32 = 50_010;
/// Edit → Auto-Completion → the next function-parameter hint.
pub const IDM_EDIT_FUNCCALLTIP_NEXT: i32 = 50_011;

/// The Search menu.
pub const IDM_SEARCH: i32 = IDM + 3000;
/// Search → Find.
pub const IDM_SEARCH_FIND: i32 = IDM_SEARCH + 1;
/// Search → Find Next.
pub const IDM_SEARCH_FINDNEXT: i32 = IDM_SEARCH + 2;
/// Search → Replace.
pub const IDM_SEARCH_REPLACE: i32 = IDM_SEARCH + 3;
/// Search → Go To (a line).
pub const IDM_SEARCH_GOTOLINE: i32 = IDM_SEARCH + 4;
/// Search → Find in Files.
pub const IDM_SEARCH_FINDINFILES: i32 = IDM_SEARCH + 13;

/// The View menu.
pub const IDM_VIEW: i32 = IDM + 4000;
/// View → Word wrap, a toggle.
pub const IDM_VIEW_WRAP: i32 = IDM_VIEW + 22;
/// View → Zoom → Zoom In.
pub const IDM_VIEW_ZOOMIN: i32 = IDM_VIEW + 23;
/// View → Zoom → Zoom Out.
pub const IDM_VIEW_ZOOMOUT: i32 = IDM_VIEW + 24;
/// View → Zoom → back to zoom level 0; Code++ calls the command
/// "reset".
pub const IDM_VIEW_ZOOMRESTORE: i32 = IDM_VIEW + 33;
/// Move the document to the other view: the lowest of the ids
/// Notepad++ numbers below its menu sections, after
/// [`IDM_FILE_PRINTNOW`]. Every other is at or above it: five more
/// View commands up to 10006, its Window menu's from 11001, its tab
/// drop-down list's from 14001, and the ranges it gives macros, Run
/// menu commands and plugin commands, from 20000 to 24999. Code++ maps
/// none of them, and numbers its own Win32 commands below this one and
/// off Print Now, for the reason given there.
pub const IDM_VIEW_GOTO_ANOTHER_VIEW: i32 = 10_001;

/// The Encoding menu. The commands below are its "Encode in" ones, which
/// set the current buffer's encoding; the names mislead about the
/// byte-order mark, so each says which. `ui_win32` maps them onto
/// Code++'s own Encoding commands, which keep the text and change the
/// encoding it is saved in; Code++ does not implement its ANSI command
/// yet, so Encode in ANSI does nothing.
pub const IDM_FORMAT: i32 = IDM + 5000;
/// Encode in ANSI.
pub const IDM_FORMAT_ANSI: i32 = IDM_FORMAT + 4;
/// Encode in UTF-8 *with* the byte-order mark, despite the name.
pub const IDM_FORMAT_UTF_8: i32 = IDM_FORMAT + 5;
/// Encode in UTF-16 BE, with the byte-order mark.
pub const IDM_FORMAT_UTF_16BE: i32 = IDM_FORMAT + 6;
/// Encode in UTF-16 LE, with the byte-order mark.
pub const IDM_FORMAT_UTF_16LE: i32 = IDM_FORMAT + 7;
/// Encode in UTF-8 *without* the byte-order mark.
pub const IDM_FORMAT_AS_UTF_8: i32 = IDM_FORMAT + 8;

/// The Language menu.
pub const IDM_LANG: i32 = IDM + 6000;

/// Notepad++'s `IDM_LANG_USER`: the command its Language menu's User
/// Defined Language rows count from, which `NPPM_GETNBUSERLANG` hands a
/// plugin. `NPPM_MENUCOMMAND` with `IDM_LANG_USER + 1 + i` switches the
/// current buffer to the `i`-th UDL in the order the host lists them,
/// for every `i` that keeps the command below [`IDM_LANG_USER_LIMIT`].
/// `IDM_LANG_USER` itself is Notepad++'s unnamed User Defined Language,
/// which Code++ does not have.
pub const IDM_LANG_USER: i32 = IDM_LANG + 180;

/// Notepad++'s `IDM_LANG_USER_LIMIT`: the User Defined Language commands
/// are the ids strictly between [`IDM_LANG_USER`] and this one.
pub const IDM_LANG_USER_LIMIT: i32 = IDM_LANG + 210;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every constant above, by name, with Notepad++'s number for it
    /// (its `menuCmdID.h`), so a regression fails without the network.
    /// The comparison that found the old numbers wrong is
    /// `tools/npp-abi-check/check.py`, which reads upstream's header;
    /// this pins only what it last agreed with. Each constant is listed,
    /// section bases included: a mapping test that compares a constant
    /// with itself cannot see a wrong number.
    const PINNED: &[(&str, i32, i32)] = &[
        ("IDM", IDM, 40_000),
        ("IDM_FILE", IDM_FILE, 41_000),
        ("IDM_FILE_NEW", IDM_FILE_NEW, 41_001),
        ("IDM_FILE_OPEN", IDM_FILE_OPEN, 41_002),
        ("IDM_FILE_CLOSE", IDM_FILE_CLOSE, 41_003),
        ("IDM_FILE_CLOSEALL", IDM_FILE_CLOSEALL, 41_004),
        ("IDM_FILE_SAVE", IDM_FILE_SAVE, 41_006),
        ("IDM_FILE_SAVEALL", IDM_FILE_SAVEALL, 41_007),
        ("IDM_FILE_SAVEAS", IDM_FILE_SAVEAS, 41_008),
        ("IDM_FILE_PRINT", IDM_FILE_PRINT, 41_010),
        ("IDM_FILE_EXIT", IDM_FILE_EXIT, 41_011),
        ("IDM_FILE_RELOAD", IDM_FILE_RELOAD, 41_014),
        ("IDM_FILE_RENAME", IDM_FILE_RENAME, 41_017),
        ("IDM_FILE_PRINTNOW", IDM_FILE_PRINTNOW, 1_001),
        ("IDM_EDIT", IDM_EDIT, 42_000),
        ("IDM_EDIT_CUT", IDM_EDIT_CUT, 42_001),
        ("IDM_EDIT_COPY", IDM_EDIT_COPY, 42_002),
        ("IDM_EDIT_UNDO", IDM_EDIT_UNDO, 42_003),
        ("IDM_EDIT_REDO", IDM_EDIT_REDO, 42_004),
        ("IDM_EDIT_PASTE", IDM_EDIT_PASTE, 42_005),
        ("IDM_EDIT_DELETE", IDM_EDIT_DELETE, 42_006),
        ("IDM_EDIT_SELECTALL", IDM_EDIT_SELECTALL, 42_007),
        ("IDM_EDIT_AUTOCOMPLETE", IDM_EDIT_AUTOCOMPLETE, 50_000),
        (
            "IDM_EDIT_AUTOCOMPLETE_CURRENTFILE",
            IDM_EDIT_AUTOCOMPLETE_CURRENTFILE,
            50_001,
        ),
        ("IDM_EDIT_FUNCCALLTIP", IDM_EDIT_FUNCCALLTIP, 50_002),
        (
            "IDM_EDIT_AUTOCOMPLETE_PATH",
            IDM_EDIT_AUTOCOMPLETE_PATH,
            50_006,
        ),
        (
            "IDM_EDIT_FUNCCALLTIP_PREVIOUS",
            IDM_EDIT_FUNCCALLTIP_PREVIOUS,
            50_010,
        ),
        (
            "IDM_EDIT_FUNCCALLTIP_NEXT",
            IDM_EDIT_FUNCCALLTIP_NEXT,
            50_011,
        ),
        ("IDM_SEARCH", IDM_SEARCH, 43_000),
        ("IDM_SEARCH_FIND", IDM_SEARCH_FIND, 43_001),
        ("IDM_SEARCH_FINDNEXT", IDM_SEARCH_FINDNEXT, 43_002),
        ("IDM_SEARCH_REPLACE", IDM_SEARCH_REPLACE, 43_003),
        ("IDM_SEARCH_GOTOLINE", IDM_SEARCH_GOTOLINE, 43_004),
        ("IDM_SEARCH_FINDINFILES", IDM_SEARCH_FINDINFILES, 43_013),
        ("IDM_VIEW", IDM_VIEW, 44_000),
        ("IDM_VIEW_WRAP", IDM_VIEW_WRAP, 44_022),
        ("IDM_VIEW_ZOOMIN", IDM_VIEW_ZOOMIN, 44_023),
        ("IDM_VIEW_ZOOMOUT", IDM_VIEW_ZOOMOUT, 44_024),
        ("IDM_VIEW_ZOOMRESTORE", IDM_VIEW_ZOOMRESTORE, 44_033),
        (
            "IDM_VIEW_GOTO_ANOTHER_VIEW",
            IDM_VIEW_GOTO_ANOTHER_VIEW,
            10_001,
        ),
        ("IDM_FORMAT", IDM_FORMAT, 45_000),
        ("IDM_FORMAT_ANSI", IDM_FORMAT_ANSI, 45_004),
        ("IDM_FORMAT_UTF_8", IDM_FORMAT_UTF_8, 45_005),
        ("IDM_FORMAT_UTF_16BE", IDM_FORMAT_UTF_16BE, 45_006),
        ("IDM_FORMAT_UTF_16LE", IDM_FORMAT_UTF_16LE, 45_007),
        ("IDM_FORMAT_AS_UTF_8", IDM_FORMAT_AS_UTF_8, 45_008),
        ("IDM_LANG", IDM_LANG, 46_000),
        ("IDM_LANG_USER", IDM_LANG_USER, 46_180),
        ("IDM_LANG_USER_LIMIT", IDM_LANG_USER_LIMIT, 46_210),
    ];

    #[test]
    fn the_ids_are_notepad_plus_pluses() {
        for &(name, ours, theirs) in PINNED {
            assert_eq!(ours, theirs, "{name}");
        }
    }

    /// A constant added to this module is pinned by number too: the
    /// names `PINNED` lists are exactly the `pub const`s defined above.
    #[test]
    fn every_id_is_pinned() {
        let source = include_str!("idm.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or("");
        let defined: BTreeSet<&str> = production
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub const "))
            .filter_map(|rest| rest.split(':').next())
            .collect();
        let pinned: BTreeSet<&str> = PINNED.iter().map(|&(name, _, _)| name).collect();
        assert!(!defined.is_empty(), "no constants found in idm.rs");
        assert_eq!(defined, pinned);
    }

    /// Every id has a number of its own, so no two can be mapped onto
    /// one menu item. `ui_win32` matches on them, where a duplicate would
    /// be an unreachable arm that only a Windows build would report.
    #[test]
    fn no_two_ids_share_a_number() {
        let mut seen = std::collections::HashMap::new();
        for &(name, value, _) in PINNED {
            if let Some(other) = seen.insert(value, name) {
                panic!("{name} and {other} are both {value}");
            }
        }
    }

    /// No Notepad++ id is a plugin command id, so a plugin that names
    /// one cannot reach a plugin's command instead, as one naming an
    /// auto-completion command did until Phase 5 (DESIGN.md §7.4).
    #[test]
    fn no_id_is_a_plugin_command_id() {
        for &(name, value, _) in PINNED {
            assert!(!crate::is_plugin_cmd_id(value), "{name} ({value})");
        }
    }
}
