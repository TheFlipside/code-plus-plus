//! Names for the message numbers a plugin sends, so the report reads
//! `NPPM_GETCURRENTSCINTILLA x2` rather than `2028 x2`.
//!
//! Both tables are read from headers already in the tree rather than
//! typed out again here, so they cannot drift from what they name:
//!
//! - `NPPM_*` from `plugins/nppcompat-headers/Notepad_plus_msgs.h`,
//!   the header plugin authors compile against. Its numbers are held
//!   against the dispatcher's by `plugin-host`'s lock test and against
//!   Notepad++'s by `tools/npp-abi-check`, so a name here is the name a
//!   plugin's source uses for that number. Code++'s own extension band
//!   (`CODEPPM_*`) comes from the dispatcher's constants.
//! - `SCI_*` from the vendored Scintilla's `Scintilla.h`.
//!
//! A number neither table knows is shown as an offset from the base of
//! its range (`RUNCOMMAND_USER+6`), which is how Notepad++'s own header
//! writes it. That is the common case for a message Code++ does not
//! implement: the compat header declares only what Code++ answers.

use std::collections::HashMap;
use std::sync::OnceLock;

use codepp_plugin_host::{
    CODEPPMSG, CODEPPMSG_RANGE, CODEPPM_EXPORTSAVEDIALOG, CODEPPM_GETZOOMLEVEL,
    CODEPPM_SETCLIPBOARD, NPPMSG, NPPMSG_RANGE, RUNCOMMAND_RANGE, RUNCOMMAND_USER,
};

/// The compat header, the vocabulary for `NPPM_*` names.
const COMPAT_HEADER: &str = include_str!("../../../plugins/nppcompat-headers/Notepad_plus_msgs.h");

/// The vendored Scintilla header, the vocabulary for `SCI_*` names.
const SCINTILLA_HEADER: &str =
    include_str!("../../../crates/scintilla-sys/vendor/scintilla/include/Scintilla.h");

/// `WM_USER`: the bottom of the range a message number is shown
/// relative to when no table names it.
const WM_USER: u32 = 0x0400;

/// The first Scintilla message number: `SCI_START` in `Scintilla.h`.
/// The host window's subclass of the views counts messages from here
/// to [`SCI_END`] as a plugin's Scintilla traffic.
pub const SCI_FIRST: u32 = 2000;
/// One past the last Scintilla message number counted. `Scintilla.h`
/// numbers its messages in three bands — `SCI_START` (2000),
/// `SCI_OPTIONAL_START` (3000) and `SCI_LEXER_START` (4000) — each
/// below the next band's start, so 5000 is one past the lexer band.
pub const SCI_END: u32 = 5000;

/// The name of a host message: one of the three ranges the dispatcher
/// owns, or any other `WM_USER` message a plugin sends the host window.
#[must_use]
pub fn host_message_name(msg: u32) -> String {
    if let Some(name) = host_names().get(&msg) {
        return (*name).to_string();
    }
    if (NPPMSG..NPPMSG + NPPMSG_RANGE).contains(&msg) {
        format!("NPPMSG+{}", msg - NPPMSG)
    } else if (RUNCOMMAND_USER..RUNCOMMAND_USER + RUNCOMMAND_RANGE).contains(&msg) {
        format!("RUNCOMMAND_USER+{}", msg - RUNCOMMAND_USER)
    } else if (CODEPPMSG..CODEPPMSG + CODEPPMSG_RANGE).contains(&msg) {
        format!("CODEPPMSG+{}", msg - CODEPPMSG)
    } else if msg >= WM_USER {
        format!("WM_USER+{}", msg - WM_USER)
    } else {
        format!("0x{msg:04X}")
    }
}

/// The name of a Scintilla message, or its number when the vendored
/// header has no `SCI_*` for it.
#[must_use]
pub fn sci_message_name(msg: u32) -> String {
    sci_names()
        .get(&msg)
        .map_or_else(|| format!("SCI#{msg}"), |name| (*name).to_string())
}

/// Whether `msg` is one the dispatcher answers or declines: inside
/// `NPPMSG`, `RUNCOMMAND_USER` or Code++'s extension band. Everything
/// else reaching the host window is the window's own business.
#[must_use]
pub fn is_host_message(msg: u32) -> bool {
    (NPPMSG..NPPMSG + NPPMSG_RANGE).contains(&msg)
        || (RUNCOMMAND_USER..RUNCOMMAND_USER + RUNCOMMAND_RANGE).contains(&msg)
        || (CODEPPMSG..CODEPPMSG + CODEPPMSG_RANGE).contains(&msg)
}

fn host_names() -> &'static HashMap<u32, &'static str> {
    static NAMES: OnceLock<HashMap<u32, &'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = HashMap::new();
        for line in COMPAT_HEADER.lines() {
            let Some((name, base, offset)) = offset_define(line) else {
                continue;
            };
            if !name.starts_with("NPPM_") {
                continue;
            }
            let base = match base {
                "NPPMSG" => NPPMSG,
                "RUNCOMMAND_USER" => RUNCOMMAND_USER,
                _ => continue,
            };
            // The header names each number once, which a test pins; were
            // it to name one twice, the first name would be shown.
            names.entry(base + offset).or_insert(name);
        }
        for (msg, name) in [
            (CODEPPM_EXPORTSAVEDIALOG, "CODEPPM_EXPORTSAVEDIALOG"),
            (CODEPPM_SETCLIPBOARD, "CODEPPM_SETCLIPBOARD"),
            (CODEPPM_GETZOOMLEVEL, "CODEPPM_GETZOOMLEVEL"),
        ] {
            names.insert(msg, name);
        }
        names
    })
}

fn sci_names() -> &'static HashMap<u32, &'static str> {
    static NAMES: OnceLock<HashMap<u32, &'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = HashMap::new();
        for line in SCINTILLA_HEADER.lines() {
            let Some((name, value)) = integer_define(line) else {
                continue;
            };
            // `SCI_START`, `SCI_OPTIONAL_START` and `SCI_LEXER_START`
            // mark the bands; they are not messages.
            if name.starts_with("SCI_") && !name.ends_with("_START") {
                names.entry(value).or_insert(name);
            }
        }
        names
    })
}

/// The name and value of `#define NAME (BASE + N)`, as the compat
/// header writes every message: `("NPPM_X", "NPPMSG", 4)`. A trailing
/// comment is allowed; anything else is not this shape.
fn offset_define(line: &str) -> Option<(&str, &str, u32)> {
    let (name, value) = define(line)?;
    let inner = value.strip_prefix('(')?;
    let (inner, rest) = inner.split_once(')')?;
    if !(rest.trim().is_empty() || rest.trim_start().starts_with("/*")) {
        return None;
    }
    let (base, offset) = inner.split_once('+')?;
    Some((name, base.trim(), offset.trim().parse().ok()?))
}

/// The name and value of `#define NAME 1234`, as `Scintilla.h` writes
/// every message.
fn integer_define(line: &str) -> Option<(&str, u32)> {
    let (name, value) = define(line)?;
    let value = value.split_whitespace().next()?;
    Some((name, value.parse().ok()?))
}

/// `#define NAME rest`, split into `NAME` and the trimmed `rest`.
fn define(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start().strip_prefix('#')?.trim_start();
    let rest = rest.strip_prefix("define")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let (name, value) = rest.trim_start().split_once(char::is_whitespace)?;
    Some((name, value.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codepp_plugin_host::dispatch::{
        NPPM_DMMREGASDCKDLG, NPPM_GETCURRENTSCINTILLA, NPPM_GETNPPDIRECTORY, NPPM_GETNPPVERSION,
    };

    #[test]
    fn host_messages_are_named_by_the_compat_header() {
        assert_eq!(
            host_message_name(NPPM_GETCURRENTSCINTILLA),
            "NPPM_GETCURRENTSCINTILLA"
        );
        assert_eq!(host_message_name(NPPM_GETNPPVERSION), "NPPM_GETNPPVERSION");
        assert_eq!(
            host_message_name(NPPM_DMMREGASDCKDLG),
            "NPPM_DMMREGASDCKDLG"
        );
        // The RUNCOMMAND_USER family resolves against its own base.
        assert_eq!(
            host_message_name(NPPM_GETNPPDIRECTORY),
            "NPPM_GETNPPDIRECTORY"
        );
        assert_eq!(
            host_message_name(CODEPPM_SETCLIPBOARD),
            "CODEPPM_SETCLIPBOARD"
        );
    }

    #[test]
    fn the_header_table_is_not_empty() {
        // A parse that silently matched nothing would leave every
        // message shown as an offset, and the first test would still
        // pass on the three `CODEPPM_*` names alone if it tested only
        // those. Pin a floor instead.
        assert!(host_names().len() > 80, "{} names", host_names().len());
        assert!(sci_names().len() > 700, "{} names", sci_names().len());
        // And every message define in the header was read: none of the
        // shape the parser expects was skipped.
        let defines = COMPAT_HEADER
            .lines()
            .filter(|l| l.trim_start().starts_with("#define NPPM_"))
            .filter(|l| l.contains("(NPPMSG") || l.contains("(RUNCOMMAND_USER"))
            .count();
        let parsed = COMPAT_HEADER
            .lines()
            .filter_map(offset_define)
            .filter(|(name, _, _)| name.starts_with("NPPM_"))
            .count();
        assert_eq!(
            parsed, defines,
            "a message define in the header was not read"
        );
    }

    #[test]
    fn the_header_names_each_message_number_once() {
        // So a number's name is never a choice between two.
        let mut seen = std::collections::HashMap::new();
        for (name, base, offset) in COMPAT_HEADER.lines().filter_map(offset_define) {
            if !name.starts_with("NPPM_") {
                continue;
            }
            if let Some(first) = seen.insert((base, offset), name) {
                panic!("{base} + {offset} is both {first} and {name}");
            }
        }
    }

    #[test]
    fn unnamed_messages_are_shown_relative_to_their_range() {
        // Upstream's numbers stop below `RUNCOMMAND_USER + 99`, so the
        // compat header names nothing there.
        assert_eq!(
            host_message_name(RUNCOMMAND_USER + 99),
            "RUNCOMMAND_USER+99"
        );
        assert_eq!(host_message_name(NPPMSG + 199), "NPPMSG+199");
        assert_eq!(host_message_name(CODEPPMSG + 15), "CODEPPMSG+15");
        assert_eq!(host_message_name(WM_USER + 7), "WM_USER+7");
        assert_eq!(host_message_name(0x0111), "0x0111");
    }

    #[test]
    fn scintilla_messages_are_named_by_the_vendored_header() {
        assert_eq!(sci_message_name(2003), "SCI_INSERTTEXT");
        assert_eq!(sci_message_name(2181), "SCI_SETTEXT");
        // The band markers are not messages.
        assert_ne!(sci_message_name(SCI_FIRST), "SCI_START");
    }

    #[test]
    fn host_message_ranges_are_the_dispatchers() {
        assert!(is_host_message(NPPMSG));
        assert!(!is_host_message(NPPMSG - 1));
        assert!(is_host_message(RUNCOMMAND_USER + RUNCOMMAND_RANGE - 1));
        assert!(!is_host_message(RUNCOMMAND_USER + RUNCOMMAND_RANGE));
        assert!(is_host_message(CODEPPMSG));
        assert!(!is_host_message(WM_USER));
    }

    #[test]
    fn defines_are_read_in_the_shapes_the_headers_use() {
        assert_eq!(
            offset_define("#define NPPM_X          (NPPMSG + 4)"),
            Some(("NPPM_X", "NPPMSG", 4))
        );
        assert_eq!(
            offset_define("#define NPPM_Y (NPPMSG + 21)  /* deprecated upstream */"),
            Some(("NPPM_Y", "NPPMSG", 21))
        );
        assert_eq!(
            offset_define("  #  define NPPM_Z (RUNCOMMAND_USER + 7)\r"),
            Some(("NPPM_Z", "RUNCOMMAND_USER", 7))
        );
        assert_eq!(offset_define("#define NPPM_W (NPPMSG + 4) + 1"), None);
        assert_eq!(offset_define("#defineNPPM_V (NPPMSG + 4)"), None);
        assert_eq!(
            integer_define("#define SCI_ADDTEXT 2001"),
            Some(("SCI_ADDTEXT", 2001))
        );
        assert_eq!(integer_define("#define SCI_X SCI_Y"), None);
    }
}
