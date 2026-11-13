//! The auto-updater's schedule and its state file (DESIGN.md §11.3).
//!
//! Headless, like the rest of `core`: this module decides *when* a check
//! for a new version is due and keeps what has to be remembered between
//! runs. It loads no library and makes no request. The libraries that
//! check, show their windows and install (WinSparkle on Windows, Sparkle
//! on macOS) are driven by the platform layers, and only when
//! [`check_is_due`] says so or the user asks.
//!
//! The state lives in `updater.xml`, next to `config.xml` and apart from
//! it: `config.xml` belongs to the UI thread, while WinSparkle reads and
//! writes its values from threads of its own (its header gives no
//! guarantee about which), so whoever owns an [`UpdaterState`] guards it
//! with a lock of its own.
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <updater last-check="1763000000">
//!   <value name="SkipThisVersion" is="0.2.1"/>
//! </updater>
//! ```
//!
//! `last-check` is Code++'s own: the time, in seconds since the Unix
//! epoch, at which it last asked a library to check. Each `<value>` is
//! one the updater library asked to have kept, under the library's own
//! name for it, which is how WinSparkle's state stays out of the
//! registry (`win_sparkle_set_config_methods`). A value is an attribute
//! and not the element's text because quick-xml drops text that is all
//! whitespace as it reads it: kept as text, a value of three spaces came
//! back empty.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// How long after a check the next one is due: one day.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// The most bytes [`UpdaterState::read_from`] reads. The file Code++
/// writes is a few hundred bytes; the cap bounds what a file put there by
/// anything else can cost.
pub const MAX_STATE_FILE_BYTES: u64 = 64 * 1024;

/// The most values a state keeps. WinSparkle 0.9.4 keeps about half a
/// dozen.
pub const MAX_STATE_VALUES: usize = 32;

/// The longest name of a value, in bytes.
pub const MAX_VALUE_NAME_BYTES: usize = 64;

/// The longest value, in bytes.
pub const MAX_VALUE_BYTES: usize = 1024;

/// Whether a scheduled check is due at `now`.
///
/// `last_check` is the time of the last check and `now` the current time,
/// both in seconds since the Unix epoch. A check is due when the updater
/// is enabled and
///
/// * no check was ever made, or
/// * [`CHECK_INTERVAL_SECS`] have passed since the last one, or
/// * the last one lies in the future, which a clock set forward and back
///   again leaves behind; waiting for that time to come could mean
///   waiting for years.
///
/// A check the user asks for from the menu does not go through this: it
/// runs whatever the preference and the time say.
#[must_use]
pub fn check_is_due(enabled: bool, last_check: Option<u64>, now: u64) -> bool {
    if !enabled {
        return false;
    }
    match last_check {
        None => true,
        Some(last) if last > now => true,
        Some(last) => now - last >= CHECK_INTERVAL_SECS,
    }
}

/// What the updater remembers between runs: the contents of
/// `updater.xml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "updater")]
pub struct UpdaterState {
    /// When Code++ last asked a library to check, in seconds since the
    /// Unix epoch. `None` until the first check.
    #[serde(
        rename = "@last-check",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub last_check: Option<u64>,
    /// The values the updater library keeps, in the order they were first
    /// written. Private so that every write goes through [`Self::set`],
    /// which holds the bounds.
    #[serde(rename = "value", default)]
    values: Vec<StateValue>,
}

/// One value an updater library asked to have kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StateValue {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@is", default)]
    value: String,
}

/// Errors from reading or writing `updater.xml`.
#[derive(Debug)]
pub enum UpdaterStateError {
    /// I/O error reading or writing the file.
    Io(std::io::Error),
    /// The file is larger than [`MAX_STATE_FILE_BYTES`].
    TooLarge,
    /// The file did not parse. Callers log and start from the default,
    /// which costs one early check and a forgotten skipped version.
    Parse(quick_xml::DeError),
    /// Serialization failure. Effectively impossible.
    Serialize(quick_xml::SeError),
}

impl std::fmt::Display for UpdaterStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "updater.xml I/O: {e}"),
            Self::TooLarge => write!(f, "updater.xml is larger than {MAX_STATE_FILE_BYTES} bytes"),
            Self::Parse(e) => write!(f, "updater.xml parse: {e}"),
            Self::Serialize(e) => write!(f, "updater.xml serialize: {e}"),
        }
    }
}

impl std::error::Error for UpdaterStateError {}

/// Whether `name` and `value` are within what a state keeps: a name that
/// is not empty, neither of them over its length, and in neither a
/// character XML 1.0 cannot carry: a control character, U+FFFE or
/// U+FFFF. A value a library writes holds none of them.
fn is_keepable(name: &str, value: &str) -> bool {
    fn xml_carries(c: char) -> bool {
        !c.is_control() && !matches!(u32::from(c), 0xFFFE | 0xFFFF)
    }
    !name.is_empty()
        && name.len() <= MAX_VALUE_NAME_BYTES
        && value.len() <= MAX_VALUE_BYTES
        && name.chars().all(xml_carries)
        && value.chars().all(xml_carries)
}

impl UpdaterState {
    /// The value kept under `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|v| v.name == name)
            .map(|v| v.value.as_str())
    }

    /// Keep `value` under `name`, replacing what was there. Returns
    /// whether it was kept: a name or value outside the bounds
    /// ([`MAX_VALUE_NAME_BYTES`], [`MAX_VALUE_BYTES`], no control
    /// characters) is refused, and so is a new name once
    /// [`MAX_STATE_VALUES`] are held.
    pub fn set(&mut self, name: &str, value: &str) -> bool {
        if !is_keepable(name, value) {
            return false;
        }
        if let Some(existing) = self.values.iter_mut().find(|v| v.name == name) {
            value.clone_into(&mut existing.value);
            return true;
        }
        if self.values.len() >= MAX_STATE_VALUES {
            return false;
        }
        self.values.push(StateValue {
            name: name.to_owned(),
            value: value.to_owned(),
        });
        true
    }

    /// Forget the value kept under `name`. Returns whether there was one.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.values.len();
        self.values.retain(|v| v.name != name);
        self.values.len() != before
    }

    /// Hold the state to its bounds: values outside them are dropped, a
    /// name keeps the first of its values that is within them, and at most [`MAX_STATE_VALUES`]
    /// stay. A file Code++ wrote is within them already; this is for one
    /// edited by hand or written by something else.
    fn clamp(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.values
            .retain(|v| is_keepable(&v.name, &v.value) && seen.insert(v.name.clone()));
        self.values.truncate(MAX_STATE_VALUES);
    }

    /// Read a state from `reader`, which the caller has opened; at most
    /// [`MAX_STATE_FILE_BYTES`] are read. An empty or blank file is the
    /// default state. What is read is held to the bounds [`Self::set`]
    /// keeps.
    ///
    /// The caller opens the file because opening one safely is the
    /// platform's business (`codepp_platform::open_regular_file`), and a
    /// missing file is the caller's first-run case.
    ///
    /// # Errors
    ///
    /// [`UpdaterStateError::Io`] when the read fails or the bytes are not
    /// UTF-8, [`UpdaterStateError::TooLarge`] past the cap, and
    /// [`UpdaterStateError::Parse`] when the document does not parse.
    pub fn read_from(reader: impl std::io::Read) -> Result<Self, UpdaterStateError> {
        use std::io::Read;
        let mut bytes = Vec::new();
        reader
            .take(MAX_STATE_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(UpdaterStateError::Io)?;
        if bytes.len() as u64 > MAX_STATE_FILE_BYTES {
            return Err(UpdaterStateError::TooLarge);
        }
        let contents = String::from_utf8(bytes).map_err(|e| {
            UpdaterStateError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        if contents.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut state: Self =
            quick_xml::de::from_str(&contents).map_err(UpdaterStateError::Parse)?;
        state.clamp();
        Ok(state)
    }

    /// Atomic write to `path`: a temporary file beside it, synced and
    /// renamed into place, as [`crate::preferences::Preferences::save`]
    /// writes `config.xml`.
    ///
    /// # Errors
    ///
    /// [`UpdaterStateError::Serialize`] for a quick-xml emit failure and
    /// [`UpdaterStateError::Io`] for any filesystem failure on the way.
    pub fn save(&self, path: &Path) -> Result<(), UpdaterStateError> {
        let xml = crate::xml::to_document(self).map_err(UpdaterStateError::Serialize)?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent).map_err(UpdaterStateError::Io)?;
        }
        let parent_dir = parent.unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".updater-")
            .suffix(".xml.tmp")
            .tempfile_in(parent_dir)
            .map_err(UpdaterStateError::Io)?;
        tmp.write_all(xml.as_bytes())
            .map_err(UpdaterStateError::Io)?;
        tmp.as_file_mut()
            .sync_all()
            .map_err(UpdaterStateError::Io)?;
        tmp.persist(path)
            .map_err(|e| UpdaterStateError::Io(e.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = CHECK_INTERVAL_SECS;
    const NOW: u64 = 1_763_000_000;

    #[test]
    fn a_disabled_updater_is_never_due() {
        for last in [None, Some(0), Some(NOW - 10 * DAY), Some(NOW + DAY)] {
            assert!(!check_is_due(false, last, NOW), "{last:?}");
        }
    }

    #[test]
    fn the_first_check_is_due_at_once() {
        assert!(check_is_due(true, None, NOW));
    }

    #[test]
    fn a_check_is_due_once_a_day_has_passed_and_not_before() {
        assert!(!check_is_due(true, Some(NOW), NOW));
        assert!(!check_is_due(true, Some(NOW - (DAY - 1)), NOW));
        assert!(check_is_due(true, Some(NOW - DAY), NOW));
        assert!(check_is_due(true, Some(NOW - 30 * DAY), NOW));
    }

    #[test]
    fn a_last_check_in_the_future_is_due() {
        assert!(check_is_due(true, Some(NOW + 1), NOW));
        assert!(check_is_due(true, Some(u64::MAX), NOW));
    }

    #[test]
    fn values_are_set_replaced_and_removed() {
        let mut s = UpdaterState::default();
        assert_eq!(s.get("SkipThisVersion"), None);
        assert!(s.set("SkipThisVersion", "0.2.1"));
        assert!(s.set("LastCheckTime", "1763000000"));
        assert_eq!(s.get("SkipThisVersion"), Some("0.2.1"));
        assert!(s.set("SkipThisVersion", "0.2.2"));
        assert_eq!(s.get("SkipThisVersion"), Some("0.2.2"));
        assert_eq!(s.values.len(), 2, "a replaced value is not kept twice");
        assert!(s.remove("SkipThisVersion"));
        assert!(!s.remove("SkipThisVersion"));
        assert_eq!(s.get("SkipThisVersion"), None);
        assert_eq!(s.get("LastCheckTime"), Some("1763000000"));
    }

    #[test]
    fn an_empty_value_is_kept() {
        let mut s = UpdaterState::default();
        assert!(s.set("Name", ""));
        assert_eq!(s.get("Name"), Some(""));
    }

    #[test]
    fn values_outside_the_bounds_are_refused() {
        let mut s = UpdaterState::default();
        assert!(!s.set("", "v"), "an empty name");
        assert!(!s.set(&"n".repeat(MAX_VALUE_NAME_BYTES + 1), "v"));
        assert!(s.set(&"n".repeat(MAX_VALUE_NAME_BYTES), "v"));
        assert!(!s.set("long", &"v".repeat(MAX_VALUE_BYTES + 1)));
        assert!(s.set("long", &"v".repeat(MAX_VALUE_BYTES)));
        assert!(!s.set("na\nme", "v"), "a control character in the name");
        assert!(
            !s.set("name", "a\u{0}b"),
            "a control character in the value"
        );
        for c in [0xFFFE, 0xFFFF] {
            let c = char::from_u32(c).expect("a scalar value");
            assert!(!s.set("name", &format!("a{c}b")), "XML cannot carry {c:?}");
            assert!(!s.set(&format!("na{c}me"), "v"), "XML cannot carry {c:?}");
        }
        assert_eq!(s.values.len(), 2);
    }

    #[test]
    fn no_more_than_the_cap_of_values_is_kept() {
        let mut s = UpdaterState::default();
        for i in 0..MAX_STATE_VALUES {
            assert!(s.set(&format!("name{i}"), "v"));
        }
        assert!(!s.set("one-more", "v"));
        assert!(s.set("name0", "replaced"), "a held name is still written");
        assert_eq!(s.values.len(), MAX_STATE_VALUES);
    }

    #[test]
    fn a_state_round_trips_through_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("updater.xml");
        let mut s = UpdaterState {
            last_check: Some(NOW),
            ..Default::default()
        };
        assert!(s.set("SkipThisVersion", "0.2.1"));
        assert!(s.set("Empty", ""));
        assert!(s.set("Text", "a <b> & \"c\" d"));
        assert!(s.set("Spaced", "  x  "), "spaces at either end are kept");
        assert!(s.set("Blank", "   "), "and so is a value of nothing else");
        assert!(s.set("NotAscii", "caf\u{e9}"));
        s.save(&path).expect("save");
        let back =
            UpdaterState::read_from(std::fs::File::open(&path).expect("open")).expect("read");
        assert_eq!(back, s);
    }

    #[test]
    fn a_state_with_nothing_in_it_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("updater.xml");
        let s = UpdaterState::default();
        s.save(&path).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(
            !raw.contains("last-check"),
            "no check yet is written as no attribute: {raw}"
        );
        let back = UpdaterState::read_from(raw.as_bytes()).expect("read");
        assert_eq!(back, s);
    }

    #[test]
    fn a_blank_file_is_the_default_state() {
        for raw in ["", "  \r\n\t"] {
            assert_eq!(
                UpdaterState::read_from(raw.as_bytes()).expect("read"),
                UpdaterState::default()
            );
        }
    }

    #[test]
    fn a_file_that_does_not_parse_is_an_error() {
        for raw in [
            "<updater",
            "not xml at all",
            "<updater last-check=\"soon\"/>",
        ] {
            assert!(
                matches!(
                    UpdaterState::read_from(raw.as_bytes()),
                    Err(UpdaterStateError::Parse(_))
                ),
                "{raw}"
            );
        }
        assert!(matches!(
            UpdaterState::read_from(&[0xFF, 0xFE, 0x00][..]),
            Err(UpdaterStateError::Io(_))
        ));
    }

    #[test]
    fn a_file_over_the_cap_is_refused_and_one_at_it_is_read() {
        let open = "<updater>";
        let close = "</updater>";
        let cap = usize::try_from(MAX_STATE_FILE_BYTES).expect("fits");
        let at = format!(
            "{open}{}{close}",
            " ".repeat(cap - open.len() - close.len())
        );
        assert_eq!(at.len(), cap);
        assert_eq!(
            UpdaterState::read_from(at.as_bytes()).expect("read"),
            UpdaterState::default()
        );
        let over = format!("{at} ");
        assert!(matches!(
            UpdaterState::read_from(over.as_bytes()),
            Err(UpdaterStateError::TooLarge)
        ));
    }

    /// A file written by something else is held to the bounds `set`
    /// keeps: values outside them go, a name keeps its first value, and
    /// no more than the cap stay.
    #[test]
    fn a_read_state_is_held_to_the_bounds() {
        use std::fmt::Write as _;
        let long_name = "n".repeat(MAX_VALUE_NAME_BYTES + 1);
        let long_value = "v".repeat(MAX_VALUE_BYTES + 1);
        let raw = format!(
            "<updater last-check=\"5\">\
             <value name=\"Twice\" is=\"{long_value}\"/>\
             <value name=\"Twice\" is=\"second, and the first within the bounds\"/>\
             <value name=\"Kept\" is=\"first\"/>\
             <value name=\"Kept\" is=\"second\"/>\
             <value name=\"\" is=\"no name\"/>\
             <value name=\"{long_name}\" is=\"v\"/>\
             <value name=\"LongValue\" is=\"{long_value}\"/>\
             <value name=\"Control\" is=\"a&#x7;b\"/>\
             <value name=\"NoValue\"/>\
             <value name=\"AlsoKept\" is=\"\"></value>\
             </updater>"
        );
        let s = UpdaterState::read_from(raw.as_bytes()).expect("read");
        assert_eq!(s.last_check, Some(5));
        assert_eq!(s.get("Kept"), Some("first"));
        assert_eq!(s.get("AlsoKept"), Some(""));
        assert_eq!(s.get("NoValue"), Some(""), "no attribute is an empty value");
        assert_eq!(
            s.get("Twice"),
            Some("second, and the first within the bounds")
        );
        assert_eq!(s.values.len(), 4, "{:?}", s.values);

        let mut many = String::from("<updater>");
        for i in 0..MAX_STATE_VALUES + 5 {
            write!(many, "<value name=\"n{i}\" is=\"v\"/>").expect("write to a String");
        }
        many.push_str("</updater>");
        let s = UpdaterState::read_from(many.as_bytes()).expect("read");
        assert_eq!(s.values.len(), MAX_STATE_VALUES);
        assert_eq!(s.get("n0"), Some("v"));
        assert_eq!(s.get(&format!("n{MAX_STATE_VALUES}")), None);
    }
}
