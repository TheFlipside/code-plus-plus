//! Auto-staging the in-tree plugins into the user's plugins directory.
//!
//! Code++ ships four preinstalled plugins as `cdylib` crates, but the
//! plugin host only discovers plugins under
//! `<config_dir>/plugins/<name>/<name>.<ext>` at runtime — there is no
//! installer yet. [`stage_bundled_plugins`] bridges the gap: on startup
//! it copies the built plugin libraries sitting next to the running
//! executable into that layout, so a freshly-built (or shipped) Code++
//! finds them with no manual step.
//!
//! The copy handles two filename quirks: a `cdylib` on Unix gets a
//! `lib` prefix (`libexample_hello.so`) that the destination must drop,
//! and the destination stem must equal its directory name — the rule
//! the host's `is_plugin_dll` discovery enforces.

use std::path::Path;

/// The in-tree plugins Code++ ships preinstalled, by their cdylib
/// `[lib] name`. Each name is also the plugin's directory and stem in
/// the plugins folder (the stem-equals-dirname rule discovery enforces).
///
/// All four now stage on Linux as well as Windows: `cppexport`'s
/// cross-platform port (it routes its Save-As and clipboard sinks
/// through the host's `CODEPPM_EXPORTSAVEDIALOG` / `CODEPPM_SETCLIPBOARD`
/// extension messages instead of calling the OS directly) makes it as
/// portable as the other three.
///
/// **The list must match each plugin's `mod imp` cfg gate** — every
/// in-tree plugin gates its entry points on `any(windows, linux)`, so on
/// macOS (and any other target) they compile to empty cdylibs with none
/// of the six ABI exports. Staging one there would only make the host
/// log a failed load, so that list is empty until Phase 5's Cocoa
/// bring-up adds the macOS `mod imp` arm and populates it.
#[cfg(target_os = "windows")]
pub const BUNDLED_PLUGINS: &[&str] =
    &["example_hello", "cppmimetools", "cppconverter", "cppexport"];

#[cfg(target_os = "linux")]
pub const BUNDLED_PLUGINS: &[&str] =
    &["example_hello", "cppmimetools", "cppconverter", "cppexport"];

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub const BUNDLED_PLUGINS: &[&str] = &[];

/// cdylib output filename prefix: `lib` on Unix, empty on Windows.
#[cfg(unix)]
const DL_PREFIX: &str = "lib";
#[cfg(not(unix))]
const DL_PREFIX: &str = "";

/// Copy the built plugin cdylibs sitting next to the running executable
/// into `<plugins_dir>/<name>/<name>.<ext>`, so a freshly-built Code++
/// finds them without a manual install step. A plugin is (re)staged
/// when its destination is missing or older than the source; up-to-date
/// destinations are left alone. Returns the number staged this call.
///
/// Runs before the host discovers or loads any plugin, so overwriting a
/// destination is safe — nothing has mapped it yet.
///
/// Non-fatal throughout: a missing source (the app was built without the
/// plugins), an unwritable config dir, or a copy failure is logged and
/// skipped, never propagated — a user with no plugins still gets a
/// working editor.
#[must_use]
pub fn stage_bundled_plugins() -> usize {
    let Some(src_dir) = crate::program_dir() else {
        tracing::debug!("plugin staging: cannot resolve the executable directory");
        return 0;
    };
    let Some(plugins_dir) = crate::plugins_dir() else {
        tracing::debug!("plugin staging: cannot resolve the plugins directory");
        return 0;
    };
    let ext = crate::PLUGIN_EXTENSION;
    let mut staged = 0;
    for name in BUNDLED_PLUGINS {
        let src = src_dir.join(format!("{DL_PREFIX}{name}.{ext}"));
        if !src.exists() {
            // The app was built without this plugin (built alone rather
            // than `--workspace`); nothing to stage.
            continue;
        }
        let dest = plugins_dir.join(name).join(format!("{name}.{ext}"));
        if !should_stage(&src, &dest) {
            continue;
        }
        if let Some(parent) = dest.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                tracing::warn!(?err, dir = ?parent, "plugin staging: mkdir failed");
                continue;
            }
        }
        // Copy to a sibling temp file, then rename it into place. A rename
        // is atomic on the same filesystem, so a *second* Code++ instance
        // that has already `dlopen`'d the destination never observes a
        // half-written file through its mmap — matching the
        // write-to-temp-then-rename discipline used for session/FIF writes
        // elsewhere. The temp name carries the pid so two instances
        // staging at once don't clobber each other's temp.
        let mut tmp = dest.clone().into_os_string();
        tmp.push(format!(".staging.{}", std::process::id()));
        let tmp = std::path::PathBuf::from(tmp);
        let result = std::fs::copy(&src, &tmp).and_then(|_| std::fs::rename(&tmp, &dest));
        match result {
            Ok(()) => {
                tracing::info!(plugin = name, dest = ?dest, "staged bundled plugin");
                staged += 1;
            }
            Err(err) => {
                // Clean up the temp on failure so a later run isn't
                // confused by a stale partial copy.
                let _ = std::fs::remove_file(&tmp);
                tracing::warn!(?err, plugin = name, "plugin staging: copy failed");
            }
        }
    }
    staged
}

/// Stage when the destination is absent, or older than the source. Any
/// metadata error errs toward staging — a fresh copy is cheap and
/// correct; a silently stale copy is a bug.
///
/// Uses a strict `>` on mtimes, so a rebuild that lands in the same
/// coarse-granularity (e.g. 1-second) tick as a prior stage won't
/// restage — acceptable for dev iteration since a clean rebuild wipes
/// `target/` and the next distinguishable-mtime build restages anyway.
fn should_stage(src: &Path, dest: &Path) -> bool {
    let Ok(dest_meta) = std::fs::metadata(dest) else {
        return true; // missing or unreadable → stage
    };
    let (Ok(src_mtime), Ok(dest_mtime)) = (
        std::fs::metadata(src).and_then(|m| m.modified()),
        dest_meta.modified(),
    ) else {
        return true; // can't compare mtimes → stage
    };
    src_mtime > dest_mtime
}

#[cfg(test)]
mod tests {
    use super::should_stage;
    use std::fs;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        let mut base = std::env::temp_dir();
        // Unique-enough per test process without relying on rand: the
        // pid plus a monotonic counter file isn't needed — a single dir
        // per test suffices since each test uses distinct filenames.
        base.push(format!("codepp-plugstage-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn stages_when_destination_absent() {
        let dir = tmp();
        let src = dir.join("stages_when_absent_src");
        fs::write(&src, b"x").unwrap();
        let dest = dir.join("stages_when_absent_dest");
        let _ = fs::remove_file(&dest);
        assert!(should_stage(&src, &dest));
    }

    #[test]
    fn skips_when_destination_newer() {
        let dir = tmp();
        let src = dir.join("skips_src");
        let dest = dir.join("skips_dest");
        fs::write(&src, b"x").unwrap();
        // Write dest after src, so dest is at least as new.
        fs::write(&dest, b"y").unwrap();
        // A strictly-newer source stages; an equal-or-newer dest does not.
        // We can't easily force mtimes without extra deps, so assert the
        // weaker, deterministic property: a dest written after the src is
        // not older, hence not staged.
        assert!(!should_stage(&src, &dest));
    }
}
