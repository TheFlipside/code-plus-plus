//! Per-user configuration directory resolution.
//!
//! The directory is `<platform-config-base>/Code++/`, where the base
//! follows each OS's convention:
//!
//!   - **Windows:** `%APPDATA%\Code++\` (e.g.
//!     `C:\Users\<user>\AppData\Roaming\Code++\`).
//!   - **Linux / other Unix:** `$XDG_CONFIG_HOME/Code++/`, defaulting
//!     to `$HOME/.config/Code++/` when `XDG_CONFIG_HOME` is unset
//!     (XDG Base Directory Spec).
//!   - **macOS:** `$HOME/Library/Application Support/Code++/`.
//!
//! `+` is a valid filename character on NTFS, ext4/btrfs/xfs, and
//! APFS/HFS+ — no escaping or alternative form is required on any
//! supported target.
//!
//! Implemented manually rather than via the `dirs` crate because
//! `dirs` transitively pulls in `option-ext`, which is MPL-2.0 —
//! against the project's permissive-only license policy
//! (CLAUDE.md, deny.toml). The std env-var lookups are ~10 lines per
//! platform; not worth a copyleft dep.
//!
//! These functions return `Option<PathBuf>` rather than panic so a
//! sandboxed runner without the relevant env var degrades gracefully —
//! the editor falls back to in-memory state for the session.

use std::ffi::OsString;
use std::path::PathBuf;

const APP_DIR: &str = "Code++";

/// Per-user config directory. May not yet exist; callers writing to
/// it must `create_dir_all` first (or use a writer that does — e.g.
/// `core::session::Session::save_to_xml` handles the create).
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    config_base_dir().map(|d| d.join(APP_DIR))
}

#[cfg(target_os = "windows")]
fn config_base_dir() -> Option<PathBuf> {
    // CSIDL_APPDATA / FOLDERID_RoamingAppData. Set by the user's shell
    // environment; absent only in unusual sandboxed scenarios.
    config_base_dir_from_appdata(std::env::var_os("APPDATA"))
}

#[cfg(target_os = "windows")]
fn config_base_dir_from_appdata(appdata: Option<OsString>) -> Option<PathBuf> {
    appdata.filter(|v| !v.is_empty()).map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn config_base_dir() -> Option<PathBuf> {
    config_base_dir_from_home(std::env::var_os("HOME"))
}

#[cfg(target_os = "macos")]
fn config_base_dir_from_home(home: Option<OsString>) -> Option<PathBuf> {
    home.filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .map(|h| h.join("Library").join("Application Support"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn config_base_dir() -> Option<PathBuf> {
    // XDG Base Directory Specification: prefer XDG_CONFIG_HOME, fall
    // back to $HOME/.config. Match the behaviour every Linux desktop
    // tool follows so users with custom XDG layouts get respected.
    //
    // Per spec: if XDG_CONFIG_HOME is unset *or empty*, fall back to
    // $HOME/.config — an empty value is not a valid override.
    config_base_dir_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn config_base_dir_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    xdg.filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
}

/// Path to `session.xml` under [`config_dir`].
#[must_use]
pub fn session_xml_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("session.xml"))
}

/// Backup directory under [`config_dir`] — `config_dir/backup/`. Holds
/// the durable text content of every untitled buffer (and, in a future
/// iteration, every dirty saved-file buffer) so unsaved work survives
/// any clean shutdown of the app. Naming matches Notepad++'s layout:
/// each backup file is `<display_name>@<timestamp>` (no extension), so
/// the directory reads at a glance and a future migration from / to
/// N++'s actual `backup/` directory is purely a copy.
///
/// May not exist yet on first launch — callers that write here are
/// responsible for `create_dir_all` before persisting.
#[must_use]
pub fn backups_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("backup"))
}

/// Plugin directory: `config_dir/plugins/`. Phase 3's plugin host
/// enumerates `*.dll` (Windows) / `*.so` (Linux) / `*.dylib` (macOS)
/// here. May not yet exist.
#[must_use]
pub fn plugins_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("plugins"))
}

/// Per-plugin data directory: `config_dir/plugins/config/`. This is the
/// path returned to plugins via `NPPM_GETPLUGINSCONFIGDIR` — plugins
/// store user-specific data files here (matches the Notepad++ layout
/// so existing plugins find their settings without modification).
#[must_use]
pub fn plugins_config_dir() -> Option<PathBuf> {
    plugins_dir().map(|d| d.join("config"))
}

/// Path to the plain-text list of disabled plugin filenames at
/// `config_dir/plugins/config/disabled.txt`. One DLL filename per
/// line (basename only — no directory components); empty lines and
/// `#`-prefixed comment lines are ignored. The Plugin Manager UI
/// writes this file when the user toggles the per-plugin Enabled
/// checkbox; `Shell::discover_plugins` reads it after enumeration
/// to mark matching entries as disabled.
#[must_use]
pub fn disabled_plugins_path() -> Option<PathBuf> {
    plugins_config_dir().map(|d| d.join("disabled.txt"))
}

/// Path to `config.xml` under [`config_dir`]. Phase 4 wires this in
/// for user settings; Phase 2 only references it for the path layout.
#[must_use]
pub fn config_xml_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.xml"))
}

/// Path to `find_history.xml` under [`config_dir`] — the rolling
/// list of recent Find Next / Replace queries the dialog populates
/// its combobox dropdowns from.
#[must_use]
pub fn find_history_xml_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("find_history.xml"))
}

/// Path to `recent_files.xml` under [`config_dir`] — the
/// most-recent-first list of full paths for files the user has
/// closed. Powers the File → Recent Files region. Persisted next
/// to `session.xml` so it survives across launches. Config for
/// the feature (enabled flag, cap, display style) lives in
/// [`config_xml_path`], not here — a hand-edit of the tuning
/// preferences shouldn't lose the path list, and vice versa.
#[must_use]
pub fn recent_files_xml_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("recent_files.xml"))
}

/// Path to `styles.xml` under [`config_dir`] — the editor's
/// default-style configuration (font face / size / bold / italic /
/// underline / fg / bg / transparency) the Style Configurator
/// dialog reads and writes. Separate file from `session.xml` so a
/// user resetting their session doesn't lose their visual prefs
/// (and vice versa).
#[must_use]
pub fn styles_xml_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("styles.xml"))
}

/// User Defined Language directory: `config_dir/userDefineLangs/`.
/// Phase 4.6's UDL runtime scans this directory at startup and
/// loads every `*.xml` file into `LANG_TABLE`'s dynamic-id
/// space (matching N++'s scan behaviour — community UDLs from
/// `notepad-plus-plus/userDefinedLanguages` use plain `.xml`
/// filenames, not the `.udl.xml` double-suffix the preinstalled
/// Markdown fixture uses). Directory name matches Notepad++'s
/// `%APPDATA%\Notepad++\userDefineLangs` so a user migrating
/// from N++ can copy their existing UDL collection over
/// verbatim.
///
/// **Guaranteed to exist after `Shell::new` returns.** Phase 4.6
/// m1b's startup path calls `create_dir_all` on this before
/// scanning, so the folder is always present after the first
/// successful editor construction. The Language menu's "Open
/// User Defined Language folder…" action (Phase 4.6 m2) also
/// `create_dir_all`s here defensively, so a click that races
/// with a between-boots directory deletion still opens Explorer
/// at a valid path.
#[must_use]
pub fn user_define_langs_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("userDefineLangs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_ends_with_app_name() {
        // `config_base_dir` returns Some on every supported platform
        // when the test runner has the relevant env var set; on
        // hermetic CI without one we skip rather than fail.
        let Some(dir) = config_dir() else {
            return;
        };
        assert_eq!(dir.file_name().and_then(|s| s.to_str()), Some(APP_DIR));
    }

    #[test]
    fn session_xml_lives_under_config_dir() {
        let Some(session) = session_xml_path() else {
            return;
        };
        let Some(config) = config_dir() else {
            return;
        };
        assert!(session.starts_with(&config));
        assert_eq!(
            session.file_name().and_then(|s| s.to_str()),
            Some("session.xml")
        );
    }

    #[test]
    fn plugins_dir_lives_under_config_dir() {
        let Some(plugins) = plugins_dir() else {
            return;
        };
        let Some(config) = config_dir() else {
            return;
        };
        assert!(plugins.starts_with(&config));
        assert_eq!(
            plugins.file_name().and_then(|s| s.to_str()),
            Some("plugins")
        );
    }

    #[test]
    fn user_define_langs_dir_lives_under_config_dir() {
        let Some(udl_dir) = user_define_langs_dir() else {
            return;
        };
        let Some(config) = config_dir() else {
            return;
        };
        assert!(udl_dir.starts_with(&config));
        // Directory name matches Notepad++ exactly so a user
        // migrating from N++ can copy their UDL collection
        // verbatim — the pin catches a silent rename.
        assert_eq!(
            udl_dir.file_name().and_then(|s| s.to_str()),
            Some("userDefineLangs")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_config_dir_lives_under_appdata() {
        let Some(dir) = config_dir() else {
            return;
        };
        let dir_str = dir.to_string_lossy();
        // Should contain "AppData\Roaming" on Windows. Assert via
        // case-insensitive contains since drive-letter casing varies.
        assert!(
            dir_str.to_lowercase().contains("appdata"),
            "expected config dir under %APPDATA%, got: {dir_str}"
        );
    }

    // Tests for the XDG resolution helper take env values as
    // parameters rather than calling `std::env::set_var`, which is
    // unsound under the multi-threaded test runner (Rust 2024 makes
    // this an unsafe operation; even on 2021 it can race with other
    // threads reading the env). The pure helper is therefore the
    // contract surface, and the production wrapper is exercised by
    // the broader integration tests above.

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_set_overrides_home() {
        let xdg = Some(OsString::from("/tmp/codepp-test-xdg"));
        let home = Some(OsString::from("/home/alice"));
        let base = config_base_dir_from(xdg, home).unwrap();
        assert_eq!(base, PathBuf::from("/tmp/codepp-test-xdg"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_unset_falls_back_to_home_dot_config() {
        let base = config_base_dir_from(None, Some(OsString::from("/home/alice"))).unwrap();
        assert_eq!(base, PathBuf::from("/home/alice/.config"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_empty_falls_back_to_home_dot_config() {
        // Per XDG spec: an empty XDG_CONFIG_HOME is not a valid
        // override; treat as unset and use $HOME/.config.
        let base = config_base_dir_from(
            Some(OsString::from("")),
            Some(OsString::from("/home/alice")),
        )
        .unwrap();
        assert_eq!(base, PathBuf::from("/home/alice/.config"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn no_env_at_all_returns_none() {
        let base = config_base_dir_from(None, None);
        assert!(base.is_none());
    }
}
