//! "Trim Trailing Spaces on Save": what a plugin does from
//! `NPPN_FILEBEFORESAVE`.
//!
//! The host tells a plugin a buffer is about to be saved *before* it reads
//! the buffer, and answers the plugin's `NPPM_*` and `SCI_*` from inside
//! that notification. So an edit made here is in the file the save writes,
//! and the buffer is clean afterwards: the contract Notepad++ plugins that
//! tidy a file on save are written against (DESIGN.md §7.4). The command
//! is a toggle, off until the user ticks it, because a sample plugin has
//! no business rewriting files nobody asked it to.

use codepp_plugin_sdk as sdk;
use std::sync::atomic::{AtomicBool, Ordering};

/// `SCI_GETCHARAT(pos)`: the byte at a document position.
const SCI_GETCHARAT: u32 = 2007;
/// `SCI_BEGINUNDOACTION` / `SCI_ENDUNDOACTION`: bracket edits that undo as
/// one.
const SCI_BEGINUNDOACTION: u32 = 2078;
const SCI_ENDUNDOACTION: u32 = 2079;
/// `SCI_GETLINEENDPOSITION(line)`: the position after a line's last
/// character, before its line end.
const SCI_GETLINEENDPOSITION: u32 = 2136;
/// `SCI_GETLINECOUNT`: the number of lines in the document.
const SCI_GETLINECOUNT: u32 = 2154;
/// `SCI_POSITIONFROMLINE(line)`: the position a line starts at.
const SCI_POSITIONFROMLINE: u32 = 2167;

/// Whether the user ticked the command.
static TRIM_ON_SAVE: AtomicBool = AtomicBool::new(false);

/// The menu command: turn trimming on or off, and tick the item to match.
pub(crate) fn toggle() {
    let on = !TRIM_ON_SAVE.load(Ordering::Relaxed);
    TRIM_ON_SAVE.store(on, Ordering::Relaxed);
    crate::imp::set_item_check(crate::imp::CMD_TRIM_ON_SAVE, on);
    sdk::set_status(if on {
        "Example Hello: trailing spaces are trimmed before each save"
    } else {
        "Example Hello: saves leave trailing spaces alone"
    });
}

/// `NPPN_FILEBEFORESAVE` for the buffer `buffer_id`: trim it, if the user
/// asked and it is the buffer the view shows.
///
/// That second check is not optional. A buffer behind another tab is
/// saved too, by Save All for one, and is announced without being brought
/// to the front, in Notepad++ as in Code++. "The current Scintilla" is
/// then some other buffer, which is not the one being saved and not this
/// plugin's to edit.
pub(crate) fn before_save(buffer_id: usize) {
    if !TRIM_ON_SAVE.load(Ordering::Relaxed) || sdk::current_buffer_id() != Some(buffer_id) {
        return;
    }
    let sci = sdk::active_scintilla();
    if sci.is_null() {
        return;
    }
    let lines = trim_trailing_blanks(sci);
    if lines > 0 {
        sdk::set_status(&format!(
            "Example Hello: trimmed {lines} line(s) before the save"
        ));
    }
}

/// Remove the spaces and tabs at the end of every line of the document in
/// `sci`, as one undo action, and return how many lines changed.
///
/// A message or three a line, which is plain to read and slow on a very
/// long file; a plugin that does this for real reads the text once.
fn trim_trailing_blanks(sci: sdk::Hwnd) -> usize {
    // SAFETY: `sci` is the host's Scintilla handle; every message here
    // takes integers but `SCI_REPLACETARGET`, whose text outlives the
    // call.
    let send = |msg: u32, wparam: usize, lparam: isize| unsafe {
        sdk::SendMessageW(sci, msg, wparam, lparam)
    };
    let line_count = usize::try_from(send(SCI_GETLINECOUNT, 0, 0)).unwrap_or(0);
    let mut trimmed = 0;
    for line in 0..line_count {
        let start = send(SCI_POSITIONFROMLINE, line, 0);
        let end = send(SCI_GETLINEENDPOSITION, line, 0);
        let mut keep = end;
        while keep > start && is_blank(send(SCI_GETCHARAT, (keep - 1) as usize, 0)) {
            keep -= 1;
        }
        if keep == end {
            continue;
        }
        if trimmed == 0 {
            send(SCI_BEGINUNDOACTION, 0, 0);
        }
        send(sdk::SCI_SETTARGETRANGE, keep as usize, end);
        send(sdk::SCI_REPLACETARGET, 0, c"".as_ptr() as isize);
        trimmed += 1;
    }
    if trimmed > 0 {
        send(SCI_ENDUNDOACTION, 0, 0);
    }
    trimmed
}

/// Whether `SCI_GETCHARAT`'s answer is a space or a tab.
fn is_blank(byte: isize) -> bool {
    matches!(byte as u8, b' ' | b'\t')
}

#[cfg(test)]
mod tests {
    use super::is_blank;

    #[test]
    fn only_spaces_and_tabs_are_trimmed() {
        assert!(is_blank(isize::from(b' ')));
        assert!(is_blank(isize::from(b'\t')));
        for other in [b'a', b'\r', b'\n', 0, 0xA0] {
            assert!(!is_blank(isize::from(other)), "{other:#x}");
        }
        // A byte Scintilla sign-extends is not a blank either.
        assert!(!is_blank(-1));
    }
}
