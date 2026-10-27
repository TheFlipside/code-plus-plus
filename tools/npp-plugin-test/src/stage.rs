//! Copying a plugin into a scratch `plugins` folder in Notepad++'s
//! layout, the only one Code++'s discovery loads (DESIGN.md §6.4).
//!
//! The harness never loads a plugin where it lies. Staging gives each
//! run a folder of its own: discovery sees exactly the plugins asked
//! for, a plugin that writes next to itself writes into the scratch
//! folder, and the copy the run maps — and Windows then keeps locked
//! until the process exits, since the host never unmaps a library — is
//! not the user's.

use std::fs;
use std::path::{Path, PathBuf};

/// How much a staged plugin folder may hold. A plugin's own folder holds
/// its DLL and perhaps documentation or a few helper libraries; anything
/// larger means the wrong folder was named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most bytes of file content.
    pub bytes: u64,
    /// The most entries — files and folders both, so a tree of empty
    /// folders is bounded too.
    pub entries: usize,
    /// The deepest a folder may sit below the plugin's own.
    pub depth: usize,
}

/// The limits [`stage`] applies.
pub const LIMITS: Limits = Limits {
    bytes: 256 * 1024 * 1024,
    entries: 10_000,
    depth: 32,
};

/// File stems Windows reserves for devices: a folder named one cannot be
/// made, so a DLL named one cannot be staged.
const RESERVED_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// One plugin, staged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    /// The file named on the command line.
    pub source: PathBuf,
    /// Where it was copied: `<plugins>/<stem>/<file>`.
    pub staged: PathBuf,
    /// Its file name.
    pub file: String,
    /// The key discovery and the rest of the host know it by.
    pub module_key: String,
}

/// Stage `dll` under `plugins_dir`, within [`LIMITS`].
///
/// A DLL that already sits in a folder named after it — Notepad++'s
/// `plugins/<name>/<name>.dll` — brings that whole folder along, since
/// a plugin may load companions from beside itself (`ComparePlus`'s
/// `libs/git2.dll`, say). Any other DLL is copied alone into a folder
/// made for it. Links inside a copied folder are skipped rather than
/// followed. The DLL itself is always copied, through a link if it is
/// one: it is the file the caller named, the same way a lone DLL is.
///
/// # Errors
///
/// The path is not a file, has no usable file name, is named like the
/// `config` folder discovery skips or like a Windows device, is already
/// staged under that name, could not be copied, or its folder is too
/// large to be a plugin's.
pub fn stage(plugins_dir: &Path, dll: &Path) -> Result<Staged, String> {
    stage_within(plugins_dir, dll, LIMITS)
}

/// [`stage`], with the limits as a parameter so tests can reach them.
fn stage_within(plugins_dir: &Path, dll: &Path, limits: Limits) -> Result<Staged, String> {
    let meta = fs::metadata(dll).map_err(|e| format!("{}: {e}", dll.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", dll.display()));
    }
    let file = dll
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no usable file name", dll.display()))?
        .to_string();
    let stem = Path::new(&file)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("{} has no file stem", dll.display()))?
        .to_string();
    check_stem(&file, &stem)?;
    let folder = plugins_dir.join(&stem);
    // `create_dir`, not `create_dir_all`: the plugins folder exists, and a
    // folder of this name already there is a second plugin of the name.
    fs::create_dir(&folder).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => {
            format!("{file}: a plugin named {stem:?} is already staged")
        }
        _ => format!("{}: {e}", folder.display()),
    })?;
    let own_folder = dll.parent().filter(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(&stem))
    });
    if let Some(source_folder) = own_folder {
        let mut used = Used::default();
        copy_children(source_folder, &folder, 0, limits, &mut used)?;
    }
    // The named file, copied through a link if it is one. In its own
    // folder the tree copy skipped it if it was a link.
    let staged = folder.join(&file);
    if !staged.is_file() {
        fs::copy(dll, &staged).map_err(|e| format!("{file}: {e}"))?;
    }
    Ok(Staged {
        source: dll.to_path_buf(),
        staged,
        module_key: codepp_core::shortcuts::module_key(&file),
        file,
    })
}

/// Refuse a stem that cannot name a plugin folder.
fn check_stem(file: &str, stem: &str) -> Result<(), String> {
    if stem.is_empty() || stem.starts_with('.') {
        return Err(format!(
            "{file}: a plugin's name cannot be empty or start with a dot"
        ));
    }
    // Windows drops a trailing dot or space from a folder's name, so the
    // folder would not match the DLL and discovery would pass it over.
    if stem.ends_with(['.', ' ']) {
        return Err(format!(
            "{file}: a plugin's name cannot end with a dot or a space"
        ));
    }
    if stem.eq_ignore_ascii_case("config") {
        return Err(format!(
            "{file}: discovery skips the plugins/Config folder, so a plugin cannot be named that"
        ));
    }
    if RESERVED_STEMS.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return Err(format!(
            "{file}: Windows reserves the name {stem:?} for a device"
        ));
    }
    Ok(())
}

/// What a folder copy has used so far.
#[derive(Default)]
struct Used {
    bytes: u64,
    entries: usize,
}

/// Copy the children of `from` into the existing folder `to`, skipping
/// links, within `limits`. `depth` is how far `from` sits below the
/// plugin's own folder.
fn copy_children(
    from: &Path,
    to: &Path,
    depth: usize,
    limits: Limits,
    used: &mut Used,
) -> Result<(), String> {
    let too_large = || {
        format!(
            "{} holds more than {} entries, {} bytes or {} levels of folders; \
             is it really one plugin's folder?",
            from.display(),
            limits.entries,
            limits.bytes,
            limits.depth
        )
    };
    let entries = fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let path = entry.path();
        // `DirEntry::file_type` does not follow a link, so a link — a
        // junction on Windows included — reads as one and is skipped.
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if !kind.is_dir() && !kind.is_file() {
            continue;
        }
        used.entries += 1;
        if used.entries > limits.entries {
            return Err(too_large());
        }
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            if depth + 1 > limits.depth {
                return Err(too_large());
            }
            fs::create_dir(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            copy_children(&path, &target, depth + 1, limits, used)?;
        } else {
            let len = entry
                .metadata()
                .map_err(|e| format!("{}: {e}", path.display()))?
                .len();
            used.bytes = used.bytes.saturating_add(len);
            if used.bytes > limits.bytes {
                return Err(too_large());
            }
            // The copy's own count, not the size read before it: a file
            // that grew in between is charged what was copied.
            let copied =
                fs::copy(&path, &target).map_err(|e| format!("{}: {e}", path.display()))?;
            used.bytes = used.bytes.saturating_add(copied.saturating_sub(len));
            if used.bytes > limits.bytes {
                return Err(too_large());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, bytes).expect("write");
    }

    #[test]
    fn a_lone_dll_gets_a_folder_of_its_own() {
        let src = tempfile::tempdir().expect("tempdir");
        let plugins = tempfile::tempdir().expect("tempdir");
        let dll = src.path().join("Downloads").join("Thing.dll");
        write(&dll, b"MZ");
        write(&src.path().join("Downloads").join("other.txt"), b"x");

        let staged = stage(plugins.path(), &dll).expect("stage");
        assert_eq!(
            staged.staged,
            plugins.path().join("Thing").join("Thing.dll")
        );
        assert_eq!(staged.file, "Thing.dll");
        assert_eq!(staged.module_key, "thing");
        assert_eq!(fs::read(&staged.staged).expect("read"), b"MZ");
        // Its neighbours are not its own.
        assert!(!plugins.path().join("Thing").join("other.txt").exists());
    }

    #[test]
    fn a_dll_in_a_folder_of_another_name_comes_alone() {
        let src = tempfile::tempdir().expect("tempdir");
        let plugins = tempfile::tempdir().expect("tempdir");
        let dll = src.path().join("Thing").join("Other.dll");
        write(&dll, b"MZ");
        write(&src.path().join("Thing").join("readme.txt"), b"x");
        let staged = stage(plugins.path(), &dll).expect("stage");
        assert_eq!(
            staged.staged,
            plugins.path().join("Other").join("Other.dll")
        );
        assert!(!plugins.path().join("Other").join("readme.txt").exists());
    }

    #[test]
    fn a_dll_in_its_own_folder_brings_the_folder() {
        let src = tempfile::tempdir().expect("tempdir");
        let plugins = tempfile::tempdir().expect("tempdir");
        let own = src.path().join("plugins").join("ComparePlus");
        write(&own.join("comparePLUS.dll"), b"MZ");
        write(&own.join("libs").join("git2.dll"), b"MZ2");

        // The folder's name matches regardless of case, as discovery's does.
        let dll = own.join("comparePLUS.dll");
        let staged = stage(plugins.path(), &dll).expect("stage");
        assert_eq!(
            staged.staged,
            plugins.path().join("comparePLUS").join("comparePLUS.dll")
        );
        assert_eq!(fs::read(&staged.staged).expect("read"), b"MZ");
        assert_eq!(
            fs::read(
                plugins
                    .path()
                    .join("comparePLUS")
                    .join("libs")
                    .join("git2.dll")
            )
            .expect("read"),
            b"MZ2"
        );
    }

    #[test]
    fn refusals_say_why() {
        let src = tempfile::tempdir().expect("tempdir");
        let plugins = tempfile::tempdir().expect("tempdir");
        assert!(stage(plugins.path(), src.path())
            .unwrap_err()
            .contains("is not a file"));
        let missing = src.path().join("missing.dll");
        assert!(stage(plugins.path(), &missing)
            .unwrap_err()
            .contains("missing.dll"));

        for name in ["CONFIG.dll", "config.dll"] {
            let dll = src.path().join(name);
            write(&dll, b"MZ");
            let why = stage(plugins.path(), &dll).unwrap_err();
            assert!(why.contains("discovery skips the plugins/Config"), "{why}");
        }
        let device = src.path().join("Nul.dll");
        // Windows cannot make this file either; staging refuses it before
        // looking, so test the stem check alone there.
        assert!(check_stem("Nul.dll", "Nul")
            .unwrap_err()
            .contains("reserves the name"));
        if cfg!(not(windows)) {
            write(&device, b"MZ");
            assert!(stage(plugins.path(), &device)
                .unwrap_err()
                .contains("reserves the name"));
        }
        assert!(check_stem("..dll", ".")
            .unwrap_err()
            .contains("start with a dot"));
        assert!(check_stem("Foo .dll", "Foo ")
            .unwrap_err()
            .contains("end with"));
        assert!(check_stem("Foo..dll", "Foo.")
            .unwrap_err()
            .contains("end with"));

        let dll = src.path().join("Twice.dll");
        write(&dll, b"MZ");
        stage(plugins.path(), &dll).expect("first");
        assert!(stage(plugins.path(), &dll)
            .unwrap_err()
            .contains("already staged"));
    }

    #[test]
    fn a_folder_past_the_limits_is_refused() {
        let src = tempfile::tempdir().expect("tempdir");
        let own = src.path().join("Big");
        write(&own.join("Big.dll"), b"MZ");
        write(&own.join("a.txt"), b"12345");
        write(&own.join("b.txt"), b"12345");
        let dll = own.join("Big.dll");
        let roomy = Limits {
            bytes: 100,
            entries: 100,
            depth: 4,
        };
        for (limits, what) in [
            (
                Limits {
                    entries: 2,
                    ..roomy
                },
                "entries",
            ),
            (Limits { bytes: 8, ..roomy }, "bytes"),
        ] {
            let plugins = tempfile::tempdir().expect("tempdir");
            let why = stage_within(plugins.path(), &dll, limits).unwrap_err();
            assert!(
                why.contains("is it really one plugin's folder"),
                "{what}: {why}"
            );
        }
        let plugins = tempfile::tempdir().expect("tempdir");
        stage_within(plugins.path(), &dll, roomy).expect("within the limits");
    }

    #[test]
    fn empty_folders_count_and_depth_is_bounded() {
        let src = tempfile::tempdir().expect("tempdir");
        let own = src.path().join("Deep");
        write(&own.join("Deep.dll"), b"MZ");
        let mut nested = own.clone();
        for level in 0..5 {
            nested = nested.join(format!("d{level}"));
        }
        fs::create_dir_all(&nested).expect("mkdir");
        let dll = own.join("Deep.dll");
        let roomy = Limits {
            bytes: 100,
            entries: 100,
            depth: 8,
        };
        for limits in [
            Limits { depth: 4, ..roomy },
            Limits {
                entries: 4,
                ..roomy
            },
        ] {
            let plugins = tempfile::tempdir().expect("tempdir");
            assert!(
                stage_within(plugins.path(), &dll, limits).is_err(),
                "{limits:?}"
            );
        }
        let plugins = tempfile::tempdir().expect("tempdir");
        stage_within(plugins.path(), &dll, roomy).expect("within the limits");
        assert!(plugins
            .path()
            .join("Deep")
            .join("d0")
            .join("d1")
            .join("d2")
            .join("d3")
            .join("d4")
            .is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_inside_a_plugin_folder_is_not_followed() {
        // A junction needs no privilege to make, unlike a symbolic link.
        let src = tempfile::tempdir().expect("tempdir");
        let outside = src.path().join("outside");
        write(&outside.join("secret.txt"), b"not the plugin's");
        let own = src.path().join("Linky");
        write(&own.join("Linky.dll"), b"MZ");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(own.join("jump"))
            .arg(&outside)
            .output()
            .expect("run mklink")
            .status;
        assert!(status.success(), "mklink /J failed");
        let plugins = tempfile::tempdir().expect("tempdir");
        stage(plugins.path(), &own.join("Linky.dll")).expect("stage");
        assert!(plugins.path().join("Linky").join("Linky.dll").is_file());
        assert!(!plugins.path().join("Linky").join("jump").exists());
    }

    #[cfg(unix)]
    #[test]
    fn links_inside_a_plugin_folder_are_skipped_but_the_named_dll_is_copied() {
        let src = tempfile::tempdir().expect("tempdir");
        let outside = src.path().join("outside");
        write(&outside.join("secret.txt"), b"not the plugin's");
        write(&outside.join("real.so"), b"ELF");
        let own = src.path().join("Linky");
        fs::create_dir_all(&own).expect("mkdir");
        std::os::unix::fs::symlink(&outside, own.join("jump")).expect("symlink");
        // The DLL named on the command line is itself a link.
        std::os::unix::fs::symlink(outside.join("real.so"), own.join("Linky.so")).expect("symlink");
        let plugins = tempfile::tempdir().expect("tempdir");
        let staged = stage(plugins.path(), &own.join("Linky.so")).expect("stage");
        assert_eq!(fs::read(&staged.staged).expect("read"), b"ELF");
        assert!(!plugins.path().join("Linky").join("jump").exists());
    }
}
