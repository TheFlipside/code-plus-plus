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
//! and the destination stem must equal its directory name — the
//! Notepad++ layout, and the only one `PluginHost::discover` loads.

use std::path::Path;

/// The in-tree plugins Code++ ships preinstalled, by their cdylib
/// `[lib] name`. Each name is also the plugin's directory and stem in
/// the plugins folder (the stem-equals-dirname rule discovery enforces).
///
/// All four stage on all three platforms. Nothing in them is
/// OS-specific: `codepp-plugin-sdk` supplies the whole FFI surface and
/// routes every `SendMessageW` through the host callback off Windows,
/// and `cppexport` — the only one that ever wanted an OS dialog or the
/// clipboard — asks the host for both through the
/// `CODEPPM_EXPORTSAVEDIALOG` / `CODEPPM_SETCLIPBOARD` extension
/// messages rather than calling the platform itself.
///
/// **The list must match each plugin's `mod imp` cfg gate.** A plugin
/// whose gate excludes the current target compiles to an *empty*
/// cdylib with none of the six ABI exports, so staging it there would
/// only make the host log a failed load. The two are kept in sync by
/// hand; `bundled_plugins_match_the_imp_gates` pins it.
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub const BUNDLED_PLUGINS: &[&str] =
    &["example_hello", "cppmimetools", "cppconverter", "cppexport"];

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
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
        if stage_one(&src, &dest) {
            tracing::info!(plugin = name, dest = ?dest, "staged bundled plugin");
            staged += 1;
        }
    }
    staged
}

/// Stage one plugin: decide whether the destination is out of date,
/// make sure the directory it goes in is a real directory, and copy
/// through an unpredictably-named temp. Returns whether it staged.
///
/// Split out from [`stage_bundled_plugins`] so the refusal paths are
/// reachable from a test — the caller resolves its paths from the
/// process environment, which a test cannot steer.
fn stage_one(src: &Path, dest: &Path) -> bool {
    if !should_stage(src, dest) {
        return false;
    }
    let Some(parent) = dest.parent() else {
        return false;
    };
    if let Err(err) = std::fs::create_dir_all(parent) {
        tracing::warn!(?err, dir = ?parent, "plugin staging: mkdir failed");
        return false;
    }
    // `create_dir_all` succeeds silently when the path already
    // resolves — including when it resolves *through* a symlink or
    // an NTFS directory junction someone planted there first. Both
    // of these paths are fixed and publicly known (four constant
    // plugin names under a constant config directory), and a
    // junction needs no privilege at all to create (`mklink /J`),
    // so this is a plant-and-wait rather than a race: the directory
    // is only ever created here, on a profile's first run. Refuse a
    // reparsed parent rather than copying a DLL through it.
    if !plugin_dir_is_safe(parent) {
        tracing::warn!(
            dir = ?parent,
            "plugin staging: refusing a linked or unreadable plugin directory"
        );
        return false;
    }
    // Copy into a randomly-named temp alongside the destination,
    // then rename it into place. The rename is the atomicity: it is
    // atomic on one filesystem, so a *second* Code++ instance that
    // has already mapped the destination never sees a half-written
    // file through its mapping.
    //
    // The temp was previously `<dest>.staging.<pid>` — deterministic,
    // since the four names are constants and a pid is visible to any
    // same-user process — and `fs::copy` follows a reparse point at
    // the destination it opens. Anyone able to write to this
    // directory could therefore pre-place that exact name as a link
    // and have the copy write through it. That is the same attack
    // `codepp_shell::fif::atomic_write` documents and fixed, and
    // this function's own comment claimed to follow that discipline
    // while reinventing the pre-fix version of it. `tempfile` picks
    // an unpredictable name and creates it `O_EXCL` (`CREATE_NEW` on
    // Windows), which fails rather than following a link.
    //
    // `persist` renames over `dest`. POSIX rename and Windows
    // `MoveFileExW` both replace a reparse point at the destination
    // rather than following it, so the destructive direction is
    // safe too.
    //
    // The bytes go through the handle `tempfile` already holds open
    // rather than through `fs::copy(src, tmp.path())`, which would
    // reopen the temp *by name* and so hand back a link-follow window
    // on the very path the unpredictable name exists to protect. The
    // window is small — the name is random and freshly created, so
    // using it would take real-time directory-change monitoring and a
    // microsecond race — but writing through the handle closes it for
    // free, and an open fd is the anchoring the rest of this
    // codebase's filesystem work aims at.
    let staged_one = tempfile::Builder::new()
        .prefix(".codepp-stage-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .and_then(|mut tmp| {
            std::io::copy(&mut std::fs::File::open(src)?, tmp.as_file_mut())?;
            tmp.persist(dest).map_err(Into::into)
        });
    match staged_one {
        Ok(_) => true,
        Err(err) => {
            // The `NamedTempFile` removes itself on drop, so a
            // failure leaves no stale partial copy behind.
            tracing::warn!(?err, dest = ?dest, "plugin staging: copy failed");
            false
        }
    }
}

/// Whether `dir` is a real directory this function may write a plugin
/// DLL into — as opposed to a symlink, an NTFS junction, or any other
/// reparse point someone planted at a path whose name was never a
/// secret.
///
/// `false` for anything that cannot be confirmed, including a stat
/// failure: the cost of refusing is that the bundled plugins are not
/// staged, which is visible and recoverable, and the cost of being
/// wrong the other way is a DLL written somewhere nobody asked for.
///
/// **One window is left open and is recorded rather than closed.**
/// This is a check on a *path*, and the `tempfile_in` that follows is
/// a second syscall on that same path, so a directory swapped between
/// the two is not caught. Closing it properly means holding a
/// directory handle and working relative to it (`openat` and
/// friends), which `std::fs` and `tempfile` do not expose portably.
/// The residual is materially harder to exploit than what it
/// replaces: a plant-and-wait against a constant path becomes a race
/// against a single instruction window at startup.
///
/// For the same reason the check stops at the leaf, a reparse point
/// planted at `<plugins_dir>` itself would be resolved through before
/// this ever runs. That is the deliberate trade named above — it is
/// also the path a user relocates on purpose — and it sits inside the
/// same-user threat model the rest of this crate assumes, where an
/// attacker with that access has more direct options anyway.
fn plugin_dir_is_safe(dir: &Path) -> bool {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => !meta.file_type().is_symlink() && !is_reparse_point(&meta),
        Err(_) => false,
    }
}

/// Whether `meta` describes a Windows reparse point that
/// `FileType::is_symlink` might not have already caught.
///
/// Belt-and-braces beside the `is_symlink` test rather than a
/// replacement for it: `is_symlink` covers symlinks and mount points,
/// while the attribute covers every other reparse tag too. The check
/// is deliberately applied to the *per-plugin leaf* directory
/// (`<plugins_dir>/<name>`) and not to the config root, so a user who
/// has legitimately relocated their whole configuration with a link
/// is unaffected — only a link at exactly the path this function is
/// about to write a DLL into is refused.
#[cfg(target_os = "windows")]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    /// `FILE_ATTRIBUTE_REPARSE_POINT`. ABI-frozen; a bare constant
    /// rather than pulling another `windows` feature in for one bit.
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Non-Windows: a symlink is the only reparse-like thing, and the
/// caller has already tested for it.
#[cfg(not(target_os = "windows"))]
fn is_reparse_point(_meta: &std::fs::Metadata) -> bool {
    false
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
    use super::{plugin_dir_is_safe, should_stage, stage_one, BUNDLED_PLUGINS};
    use std::fs;
    use std::path::PathBuf;

    /// The straight-line case, so the refusal tests below cannot pass
    /// by refusing everything.
    #[test]
    fn stage_one_copies_into_a_plain_directory() {
        let base = tmp().join("stage-plain");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let src = base.join("src.bin");
        fs::write(&src, b"plugin bytes").unwrap();
        let dest = base.join("dest-dir").join("dest.bin");
        assert!(stage_one(&src, &dest));
        assert_eq!(fs::read(&dest).unwrap(), b"plugin bytes");
        // Second call is a no-op: the destination is now current.
        assert!(!stage_one(&src, &dest));
        // And no temp is left behind.
        let leftovers: Vec<_> = fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".codepp-stage-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    /// The temp the copy writes through must not be at a name an
    /// attacker can predict.
    ///
    /// The old implementation used `<dest>.staging.<pid>` and plain
    /// `fs::copy`, which follows a link at the path it opens — so
    /// anyone able to write to the plugin directory could pre-place
    /// that exact name and have the copy land wherever they chose.
    /// A **hard link** is the sharp version on Windows because
    /// `mklink /H` needs no privilege at all, unlike a symlink; it
    /// aliases the same file data, so a copy through it rewrites the
    /// attacker's chosen file in place.
    ///
    /// This is the same attack `codepp_shell::fif::atomic_write`
    /// documents, and it is pinned the same way.
    #[test]
    fn stage_one_cannot_be_redirected_by_a_planted_temp_link() {
        let base = tmp().join("stage-planted-temp");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let src = base.join("src.bin");
        fs::write(&src, b"plugin bytes").unwrap();

        let dir = base.join("plugin-dir");
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("dest.bin");

        let victim = base.join("victim.txt");
        fs::write(&victim, b"must not be overwritten").unwrap();

        // Exactly the name the pre-fix implementation would have used.
        let predictable = {
            let mut p = dest.clone().into_os_string();
            p.push(format!(".staging.{}", std::process::id()));
            PathBuf::from(p)
        };
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, &predictable).unwrap();
        #[cfg(target_os = "windows")]
        fs::hard_link(&victim, &predictable).unwrap();

        assert!(
            stage_one(&src, &dest),
            "staging itself should still succeed"
        );
        assert_eq!(
            fs::read(&victim).unwrap(),
            b"must not be overwritten",
            "the copy was redirected through the planted temp name"
        );
        assert_eq!(fs::read(&dest).unwrap(), b"plugin bytes");
    }

    /// The call site must consult the directory guard, not merely own
    /// one. A junction planted at the per-plugin directory before the
    /// first run must leave the DLL unwritten and whatever it points at
    /// untouched.
    #[cfg(target_os = "windows")]
    #[test]
    fn stage_one_refuses_a_junctioned_plugin_directory() {
        let base = tmp().join("stage-junction");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let src = base.join("src.bin");
        fs::write(&src, b"plugin bytes").unwrap();

        // Where the attacker wants the bytes to land.
        let elsewhere = base.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let planted = base.join("plugin-dir");
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&planted)
            .arg(&elsewhere)
            .output()
            .expect("run mklink");
        assert!(
            out.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let dest = planted.join("dest.bin");
        assert!(
            !stage_one(&src, &dest),
            "staging through a junction must be refused"
        );
        assert!(
            !elsewhere.join("dest.bin").exists(),
            "a DLL was written through the planted junction"
        );
    }

    /// The ordinary case: a directory this function created itself.
    #[test]
    fn a_plain_directory_is_safe_to_stage_into() {
        let dir = tmp().join("plain-plugin-dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(plugin_dir_is_safe(&dir));
    }

    /// A path that does not exist is not safe — `stage_bundled_plugins`
    /// only reaches the check after `create_dir_all`, so a miss here
    /// means something removed it underneath us.
    #[test]
    fn a_missing_directory_is_not_safe_to_stage_into() {
        let dir = tmp().join("definitely-not-created");
        let _ = fs::remove_dir_all(&dir);
        assert!(!plugin_dir_is_safe(&dir));
    }

    /// An NTFS directory junction planted at the destination. This is
    /// the case that needs **no privilege at all** (`mklink /J`), so it
    /// runs on every Windows runner rather than being `#[ignore]`d —
    /// and it is the more practically exploitable of the two, because
    /// the per-plugin directory is created exactly once, on a profile's
    /// first run, so an attacker can plant and wait rather than race.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_directory_junction_is_refused() {
        let base = tmp();
        let target = base.join("junction-target");
        let link = base.join("junction-link");
        let _ = fs::remove_dir_all(&target);
        let _ = fs::remove_dir_all(&link);
        fs::create_dir_all(&target).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .expect("run mklink");
        assert!(
            status.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(
            !plugin_dir_is_safe(&link),
            "a junction at the plugin directory must be refused"
        );
        // The guard must not be refusing *everything* — otherwise it
        // would pass while staging nothing, forever.
        assert!(plugin_dir_is_safe(&target));
    }

    /// The symlink variant. `#[ignore]`d on Windows for the reason
    /// docs/DEVELOPMENT.md §2.6 records: creating one needs
    /// `SeCreateSymbolicLinkPrivilege`, i.e. developer mode, and a
    /// runtime skip would silently drop the coverage while still
    /// reporting green.
    #[cfg_attr(
        target_os = "windows",
        ignore = "creating a symlink needs SeCreateSymbolicLinkPrivilege; see docs/DEVELOPMENT.md §2.6"
    )]
    #[test]
    fn a_symlinked_directory_is_refused() {
        let base = tmp();
        let target = base.join("symlink-target");
        let link = base.join("symlink-link");
        let _ = fs::remove_dir_all(&target);
        let _ = fs::remove_file(&link);
        let _ = fs::remove_dir_all(&link);
        fs::create_dir_all(&target).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(target_os = "windows")]
        std::os::windows::fs::symlink_dir(&target, &link).unwrap();
        assert!(
            !plugin_dir_is_safe(&link),
            "a symlink at the plugin directory must be refused"
        );
        assert!(plugin_dir_is_safe(&target));
    }

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

    /// Every plugin in [`BUNDLED_PLUGINS`] must actually build its entry
    /// points on this target.
    ///
    /// The two live in different crates with no dependency between them,
    /// so nothing but this test connects them. Staging a plugin whose
    /// `mod imp` gate excludes the current target copies an *empty*
    /// cdylib into the user's plugins folder, where it fails to load and
    /// shows up in the Plugin Manager as a broken plugin — a silent
    /// regression the compiler cannot see.
    ///
    /// Reads each `imp.rs`'s inner gate attribute rather than trying to
    /// evaluate a `cfg`, because the gate is source in another crate.
    #[test]
    fn bundled_plugins_match_the_imp_gates() {
        // (`BUNDLED_PLUGINS` name, that plugin's `imp.rs` source.)
        let gates: &[(&str, &str)] = &[
            (
                "example_hello",
                include_str!("../../../plugins/example-hello/src/imp.rs"),
            ),
            (
                "cppmimetools",
                include_str!("../../../plugins/cppmimetools/src/imp.rs"),
            ),
            (
                "cppconverter",
                include_str!("../../../plugins/cppconverter/src/imp.rs"),
            ),
            (
                "cppexport",
                include_str!("../../../plugins/cppexport/src/imp.rs"),
            ),
        ];
        let target = std::env::consts::OS;
        for (name, src) in gates {
            let gate = src
                .lines()
                .find(|l| l.starts_with("#![cfg("))
                .unwrap_or_else(|| panic!("{name}: no inner cfg gate found in imp.rs"));
            let builds_here = gate.contains(&format!("target_os = \"{target}\""));
            let staged = BUNDLED_PLUGINS.contains(name);
            assert_eq!(
                staged, builds_here,
                "{name}: BUNDLED_PLUGINS says staged={staged} on {target}, but its \
                 imp.rs gate says builds_here={builds_here} ({gate})"
            );
        }
    }
}
