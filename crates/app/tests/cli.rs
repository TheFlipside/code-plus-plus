//! What the built `codepp` binary writes, and where, for the runs that
//! end before any window opens.
//!
//! On Windows the binary is linked for the GUI subsystem, so that
//! starting it from Explorer opens no console window beside the editor
//! (`src/console.rs` has the story). These tests pin the two halves of
//! that: the executable really is a GUI-subsystem one, and `--help` and
//! a usage error still arrive where the command line sends them.

use std::process::Command;

/// The binary under test, built by cargo for this test run.
const CODEPP: &str = env!("CARGO_BIN_EXE_codepp");

#[test]
fn help_reaches_a_pipe() {
    let out = Command::new(CODEPP)
        .arg("--help")
        .output()
        .expect("codepp starts");
    assert!(out.status.success(), "--help exits 0: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("USAGE:"), "usage on stdout, got {stdout:?}");
    assert!(stdout.contains("--perf"), "every flag listed: {stdout:?}");
}

#[test]
fn a_bad_flag_is_reported_on_stderr_with_a_failing_status() {
    let out = Command::new(CODEPP)
        .arg("--pref")
        .output()
        .expect("codepp starts");
    assert!(!out.status.success(), "a bad flag fails: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--pref"), "the flag is named: {stderr:?}");
    assert!(out.stdout.is_empty(), "nothing on stdout: {:?}", out.stdout);
}

#[cfg(windows)]
mod windows {
    use super::CODEPP;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// `IMAGE_SUBSYSTEM_WINDOWS_GUI`, from `winnt.h`. A console program
    /// carries 3 (`IMAGE_SUBSYSTEM_WINDOWS_CUI`).
    const SUBSYSTEM_GUI: u16 = 2;

    /// `CREATE_NO_WINDOW`, from `winbase.h`: the console program
    /// started gets a console with no window, so a test run from an
    /// IDE or a service puts nothing on screen.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// The `Subsystem` field of a PE file's optional header.
    fn pe_subsystem(path: &Path) -> u16 {
        let bytes = std::fs::read(path).expect("the executable is readable");
        assert!(bytes.len() >= 0x40, "too short for a DOS header");
        // `e_lfanew`, at 0x3C of the DOS header, is where the PE
        // signature starts.
        let pe = u32::from_le_bytes(bytes[0x3C..0x40].try_into().unwrap()) as usize;
        // The signature (4 bytes) and the COFF header (20) come before
        // the optional header, whose `Subsystem` is at offset 68 in
        // PE32 and PE32+ alike.
        let at = pe + 4 + 20 + 68;
        assert!(at + 2 <= bytes.len(), "too short for a PE header");
        assert_eq!(&bytes[pe..pe + 4], b"PE\0\0", "a PE signature");
        u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
    }

    /// Started from Explorer, a console-subsystem `codepp.exe` gets a
    /// console window of its own for as long as it runs.
    #[test]
    fn the_executable_is_linked_for_the_gui_subsystem() {
        assert_eq!(
            pe_subsystem(Path::new(CODEPP)),
            SUBSYSTEM_GUI,
            "codepp.exe must be a GUI-subsystem program; see src/console.rs"
        );
    }

    /// A file of this test's own under cargo's scratch folder for
    /// integration tests, removed first so a stale one cannot pass.
    fn scratch(name: &str) -> PathBuf {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Run `codepp <args>` through `cmd`, with `cmd`'s own `>` and `2>`
    /// sending the two streams to files, and answer what each holds
    /// and whether `codepp` succeeded.
    ///
    /// `cmd` matters here. It hands a redirection to the program by
    /// inheritance, and `AttachConsole` replaces handles that arrive
    /// that way with the console's (measured; `src/console.rs`). Rust's
    /// `Command` passes its pipes in the start-up information, which
    /// the attach leaves alone, so the tests above cannot see a
    /// redirect being lost. `cmd` is a console program, so it has a
    /// console for `codepp` to attach to, which is the case to test.
    ///
    /// The files are read as soon as `cmd` returns, which is sound
    /// because `cmd /c` waits for the program it runs, a GUI-subsystem
    /// one included, and exits with its status; only an interactive
    /// prompt does not wait. The status answered here is `codepp`'s
    /// own, so each caller's assertion on it would fail if `cmd` had
    /// not waited.
    fn through_cmd(args: &str, tag: &str) -> (String, String, bool) {
        let out = scratch(&format!("cli-{tag}.out"));
        let err = scratch(&format!("cli-{tag}.err"));
        // `/s` with the whole command in one pair of quotes: `cmd`
        // strips that pair and reads the rest as typed.
        let line = format!(
            "/d /s /c \"\"{CODEPP}\" {args} > \"{}\" 2> \"{}\"\"",
            out.display(),
            err.display()
        );
        let status = Command::new("cmd")
            .raw_arg(line)
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .expect("cmd starts");
        let read = |p: &Path| std::fs::read_to_string(p).expect("cmd created the file");
        (read(&out), read(&err), status.success())
    }

    #[test]
    fn help_redirected_by_cmd_lands_in_the_file() {
        let (out, err, succeeded) = through_cmd("--help", "help");
        assert!(succeeded, "--help exits 0 through cmd");
        assert!(out.contains("USAGE:"), "usage in the file, got {out:?}");
        assert!(err.is_empty(), "nothing on stderr: {err:?}");
    }

    #[test]
    fn an_error_redirected_by_cmd_lands_in_the_file() {
        let (out, err, succeeded) = through_cmd("--pref", "bad-flag");
        assert!(!succeeded, "a bad flag's failing status comes through cmd");
        assert!(err.contains("--pref"), "the error in the file, got {err:?}");
        assert!(out.is_empty(), "nothing on stdout: {out:?}");
    }
}
