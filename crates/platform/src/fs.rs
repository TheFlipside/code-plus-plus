//! Opening a file to read only when its path names a regular file, and
//! handing Windows a path it takes whatever its length.
//!
//! Reading a named pipe waits for something to write to it, and reading a
//! device such as `/dev/zero` never ends. A thread that opened one of those
//! by mistake, from a session record, the command line or a file planted in
//! the config folder, would be held for good: the file loader's one worker
//! with every open queued behind it, or the UI thread at startup.
//! [`open_regular_file`] refuses them before anything is read, and its open
//! does not wait for a writer, as a plain open of a named pipe does. It can
//! still wait on a filesystem that does not answer, as any open can.
//!
//! [`long_path`] is for code that passes a path to Windows' file calls
//! itself rather than through `std`, as tempfile's rename does.

use std::borrow::Cow;
use std::fs::File;
use std::io;
use std::path::Path;

/// Open `path` for reading if it names a regular file, or `Ok(None)` if it
/// names anything else: a folder, a named pipe, a socket or a device. A
/// caller reports a refusal as [`not_a_regular_file`].
///
/// The path is looked up first. On Unix that is a `stat`, so a pipe or a
/// device the path already names is refused without being opened at all;
/// on Windows `std`'s lookup itself opens the path, with no access rights.
/// The type is checked again on the file opened, which is the authority:
/// a path swapped for a pipe between the two is refused too. For that
/// case the open itself must not wait, and must not act on what it opens:
///
/// * Unix: `O_NONBLOCK`, since opening a named pipe for reading otherwise
///   waits *inside `open`* for a writer, and `O_NOCTTY`, since opening a
///   terminal otherwise makes it the controlling terminal of a process
///   that has none (Linux), which its hangup then signals. `O_NONBLOCK` is
///   cleared once the file is known to be a regular one, so reading it
///   behaves as it always has.
/// * Windows: opening a named pipe or a device does not wait, and the
///   handle must be a disk file (`GetFileType`), since `std`'s `is_file()`
///   there means only "neither a folder nor a link".
///
/// # Errors
///
/// What looking the path up, opening it or examining the file reports. A
/// lease being broken on the file (Linux) is reported at once as
/// [`io::ErrorKind::WouldBlock`], where an open that waited would have
/// held the caller until the lease's holder let go of it. On Windows,
/// looking up a named pipe may fail outright rather than report a pipe,
/// which is then an error rather than `Ok(None)`; either way nothing is
/// handed back. That is read from `std`'s source and not run.
pub fn open_regular_file(path: &Path) -> io::Result<Option<File>> {
    if !std::fs::metadata(path)?.is_file() {
        return Ok(None);
    }
    open_and_check(path)
}

/// The error a caller reports for a path [`open_regular_file`] refused.
#[must_use]
pub fn not_a_regular_file() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

/// The second half of [`open_regular_file`]: open `path` without waiting,
/// and hand the file back only if what was opened is a regular file.
fn open_and_check(path: &Path) -> io::Result<Option<File>> {
    let file = open_without_waiting(path)?;
    if !file.metadata()?.is_file() || !is_disk_file(&file) {
        return Ok(None);
    }
    clear_nonblocking(&file)?;
    Ok(Some(file))
}

/// Open `path` for reading without waiting on what it names and without
/// making a terminal the process's controlling one.
#[cfg(unix)]
fn open_without_waiting(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
}

/// Open `path` for reading. Opening a named pipe or a device does not wait
/// on Windows, so nothing more is needed.
#[cfg(not(unix))]
fn open_without_waiting(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Clear the `O_NONBLOCK` [`open_without_waiting`] set: reading a regular
/// file ignores it on the filesystems measured, but a filesystem may read
/// it (a file still being fetched, on a network or cloud mount), and the
/// reader expects a read that waits for its bytes.
#[cfg(unix)]
fn clear_nonblocking(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is `file`'s own descriptor, open for both calls, and
    // `F_GETFL` / `F_SETFL` take nothing else.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Nothing to clear where the open set nothing.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // the Unix twin can fail
fn clear_nonblocking(_: &File) -> io::Result<()> {
    Ok(())
}

/// Whether `file` is a disk file rather than a pipe or a character device,
/// which `std`'s metadata cannot tell apart from one on Windows.
#[cfg(windows)]
fn is_disk_file(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_DISK};
    // SAFETY: the handle is `file`'s own, open for the call, and
    // `GetFileType` only reads its type.
    unsafe { GetFileType(HANDLE(file.as_raw_handle())) == FILE_TYPE_DISK }
}

/// The `fstat` type check covers this off Windows.
#[cfg(not(windows))]
fn is_disk_file(_: &File) -> bool {
    true
}

/// The longest absolute path, in UTF-16 units, that [`long_path`] hands
/// back as it was written. Windows' file calls refuse a path of `MAX_PATH`
/// (260) units or more, its NUL included, unless it is verbatim (`\\?\`),
/// and `CreateDirectoryW` keeps 12 of those for an 8.3 name. `std` makes a
/// path verbatim before each call of its own once its absolute form, with
/// the NUL, reaches 248 units (its `LEGACY_MAX_PATH`), so the longest it
/// passes as written is 246.
#[cfg(windows)]
const PLAIN_PATH_MAX_UNITS: usize = 246;

/// `path` in a form Windows' file calls take whatever its length, with
/// room for `room` more units on it: `path` as it is while its absolute
/// form, with the room, is at most [`PLAIN_PATH_MAX_UNITS`] long, and
/// otherwise absolute and verbatim (`\\?\C:\…`, `\\?\UNC\server\share\…`),
/// which they take up to 32,767 units long.
///
/// `std` makes the same change before each call of its own; this is for
/// code that calls Windows itself. tempfile's rename is the case in point:
/// it passes the paths of the temporary file and its target to
/// `SetFileAttributesW` and `MoveFileExW` as they are, so a write through
/// it to a longer path fails with "the system cannot find the path
/// specified". A caller that writes a temporary file beside `path` gives
/// the length of that file's name as `room`, so that whenever either path
/// is too long both are in the verbatim form.
///
/// A path short enough, with the room, is handed back unchanged, so it
/// reaches Windows as it always did and an error names it as it was
/// written. One within `room` of the limit turns verbatim too, though
/// Windows would have taken it as written; that changes nothing but the
/// form an error names it in. A longer one goes through
/// `GetFullPathNameW` (`std::path::absolute`), as `std` does: `.` and `..`
/// are resolved, `/` becomes `\`, and a name's trailing dots and spaces are
/// dropped, as Windows does to a path that is not verbatim. A path already
/// verbatim, and one `std` cannot make absolute, are handed back as they
/// are.
///
/// Off Windows `path` itself: nothing there limits a path this way.
#[cfg(windows)]
#[must_use]
pub fn long_path(path: &Path, room: usize) -> Cow<'_, Path> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let Ok(absolute) = std::path::absolute(path) else {
        return Cow::Borrowed(path);
    };
    let units: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    if units.len().saturating_add(room) <= PLAIN_PATH_MAX_UNITS {
        return Cow::Borrowed(path);
    }
    match verbatim(&units) {
        Some(verbatim) => Cow::Owned(OsString::from_wide(&verbatim).into()),
        None => Cow::Owned(absolute),
    }
}

/// `path` itself: only Windows limits a path's length this way.
#[cfg(not(windows))]
#[must_use]
pub fn long_path(path: &Path, _room: usize) -> Cow<'_, Path> {
    Cow::Borrowed(path)
}

/// `absolute`, a path as `GetFullPathNameW` hands it back, in verbatim
/// form: `C:\…` as `\\?\C:\…`, `\\server\share\…` as
/// `\\?\UNC\server\share\…`, and a device path `\\.\…` as `\\?\…`, the
/// prefixes `std` gives them. `None` for a path already verbatim (`\\?\`
/// or `\??\`) and for any other form, which is left as it is.
#[cfg(any(windows, test))]
fn verbatim(absolute: &[u16]) -> Option<Vec<u16>> {
    const SEP: u16 = 0x5C; // '\'
    const COLON: u16 = 0x3A; // ':'
    const DOT: u16 = 0x2E; // '.'
    const QUERY: u16 = 0x3F; // '?'
    let (prefix, rest): (&str, &[u16]) = match absolute {
        [_, COLON, SEP, ..] => (r"\\?\", absolute),
        [SEP, SEP, DOT, SEP, rest @ ..] => (r"\\?\", rest),
        // `\\?\` and `\??\`.
        [SEP, SEP | QUERY, QUERY, SEP, ..] => return None,
        [SEP, SEP, rest @ ..] => (r"\\?\UNC\", rest),
        _ => return None,
    };
    Some(prefix.encode_utf16().chain(rest.iter().copied()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::path::PathBuf;

    #[test]
    fn a_regular_file_opens_and_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "text\n").unwrap();
        let mut file = open_regular_file(&path).unwrap().expect("a regular file");
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "text\n");
    }

    /// What a caller reports for a refused path.
    #[test]
    fn a_refusal_reads_as_not_a_regular_file() {
        let e = not_a_regular_file();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(e.to_string(), "not a regular file");
    }

    #[test]
    fn a_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open_regular_file(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_regular_file(&dir.path().join("gone")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    /// A link to a regular file is followed, as an ordinary open follows it:
    /// a file a user reaches through a link is still a file.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_regular_file_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "text\n").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(open_regular_file(&link).unwrap().is_some());
    }

    /// Make a named pipe at `path`.
    #[cfg(unix)]
    fn mkfifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path in a folder the test
        // owns; `mkfifo` has no other precondition.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// A named pipe is refused, at once rather than once something writes
    /// to it: by the lookup, and by the check on what was opened, for a
    /// pipe swapped in after the lookup. Run on a thread, so a regression
    /// fails the test instead of hanging it.
    #[cfg(unix)]
    #[test]
    fn a_named_pipe_is_refused_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let pipe = dir.path().join("pipe");
        mkfifo(&pipe);
        assert_answers_at_once(
            move || {
                (
                    open_regular_file(&pipe).unwrap().is_none(),
                    open_and_check(&pipe).unwrap().is_none(),
                )
            },
            &(true, true),
        );
    }

    /// A device is refused, by the lookup and by the check on what was
    /// opened: `/dev/null` reads as empty, and `/dev/zero` never ends.
    #[cfg(unix)]
    #[test]
    fn a_device_is_refused() {
        for device in ["/dev/null", "/dev/zero"] {
            let path = Path::new(device);
            assert!(open_regular_file(path).unwrap().is_none(), "{device}");
            assert!(open_and_check(path).unwrap().is_none(), "{device}");
        }
    }

    /// The file handed back reads as an ordinary one: `O_NONBLOCK` is gone
    /// once the open has served its purpose.
    #[cfg(unix)]
    #[test]
    fn the_file_handed_back_is_not_left_non_blocking() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "text\n").unwrap();
        let file = open_regular_file(&path).unwrap().expect("a regular file");
        // SAFETY: the descriptor is `file`'s own, open for the call.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(flags & libc::O_NONBLOCK, 0);
    }

    /// Run `f` on a thread and assert it returns `expected` within a few
    /// seconds; a call that waits on a pipe fails the test rather than
    /// hanging the suite.
    #[cfg(unix)]
    fn assert_answers_at_once<T: PartialEq + std::fmt::Debug + Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
        expected: &T,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the call waited on what the path names");
        assert_eq!(&got, expected);
    }

    /// Opening a terminal makes it no process's controlling terminal. A
    /// session leader with no controlling terminal, which Code++ is when
    /// started from a desktop launcher, otherwise takes the first terminal
    /// it opens as one (Linux), and its hangup then signals the editor.
    /// Checked in a child process of this test binary, since a test cannot
    /// become a session leader in the harness's own process.
    #[cfg(target_os = "linux")]
    #[test]
    fn opening_a_terminal_takes_no_controlling_terminal() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fs::tests::controlling_terminal_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains(CHILD_DONE),
            "child failed: {}\n{stdout}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The variable that makes [`controlling_terminal_child`] do its work.
    #[cfg(target_os = "linux")]
    const CHILD_ENV: &str = "CODEPP_PLATFORM_CTTY_CHILD";

    /// What [`controlling_terminal_child`] prints once it has checked.
    #[cfg(target_os = "linux")]
    const CHILD_DONE: &str = "controlling-terminal-check-done";

    /// The child half of [`opening_a_terminal_takes_no_controlling_terminal`]:
    /// does nothing unless [`CHILD_ENV`] is set. It starts a session with no
    /// controlling terminal, opens a terminal it made the way the loader
    /// opens a path, and checks the session still has none.
    #[cfg(target_os = "linux")]
    #[test]
    fn controlling_terminal_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        // SAFETY: `setsid` has no preconditions; it fails only for a
        // process group leader, which a child the harness spawned is not.
        assert_ne!(unsafe { libc::setsid() }, -1, "setsid failed");
        assert_eq!(controlling_terminal(), 0, "a new session has no terminal");
        // SAFETY: plain calls on a descriptor this test owns; `ptsname_r`
        // writes at most `buf.len()` bytes into `buf`.
        let slave = unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(master >= 0, "posix_openpt failed");
            assert_eq!(libc::grantpt(master), 0);
            assert_eq!(libc::unlockpt(master), 0);
            let mut buf = [0u8; 128];
            assert_eq!(
                libc::ptsname_r(master, buf.as_mut_ptr().cast(), buf.len()),
                0
            );
            let len = buf.iter().position(|&b| b == 0).unwrap();
            String::from_utf8(buf[..len].to_vec()).unwrap()
        };
        let file = open_without_waiting(Path::new(&slave)).unwrap();
        assert_eq!(
            controlling_terminal(),
            0,
            "opening {slave} took it as the terminal"
        );
        drop(file);
        println!("{CHILD_DONE}");
    }

    /// This process's controlling terminal, as the device number in
    /// `/proc/self/stat`: 0 for none.
    #[cfg(target_os = "linux")]
    fn controlling_terminal() -> i64 {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        // The name in parentheses may hold spaces; the fields after it are
        // state, ppid, pgrp, session and then tty_nr.
        let rest = &stat[stat.rfind(')').unwrap() + 2..];
        rest.split(' ').nth(4).unwrap().parse().unwrap()
    }

    /// The null device is refused, or fails to open; it is never handed back
    /// as a file to read.
    #[cfg(windows)]
    #[test]
    fn the_null_device_is_not_handed_back() {
        assert!(!matches!(open_regular_file(Path::new("NUL")), Ok(Some(_))));
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// Each absolute form gets the prefix `std` gives it, and a verbatim
    /// path, or one of no form `verbatim` knows, is left alone.
    #[test]
    fn each_absolute_form_gets_its_verbatim_prefix() {
        let cases = [
            (r"C:\a\b.txt", Some(r"\\?\C:\a\b.txt")),
            (r"\\server\share\a.txt", Some(r"\\?\UNC\server\share\a.txt")),
            (r"\\.\C:\a.txt", Some(r"\\?\C:\a.txt")),
            (r"\\?\C:\a.txt", None),
            (r"\??\C:\a.txt", None),
            (r"\\?\UNC\server\share\a.txt", None),
            (r"a\b.txt", None),
            ("", None),
        ];
        for (absolute, want) in cases {
            assert_eq!(verbatim(&wide(absolute)), want.map(wide), "{absolute}");
        }
    }

    /// Off Windows a path of any length is handed back as it is.
    #[cfg(not(windows))]
    #[test]
    fn off_windows_a_long_path_is_left_alone() {
        let path = PathBuf::from(format!("/{}/{}", "d".repeat(200), "e".repeat(200)));
        assert!(matches!(long_path(&path, 64), Cow::Borrowed(p) if p == path));
    }

    /// `C:\` and `n` more units.
    #[cfg(windows)]
    fn drive_path(n: usize) -> PathBuf {
        PathBuf::from(format!(r"C:\{}", "a".repeat(n)))
    }

    /// A path at most [`PLAIN_PATH_MAX_UNITS`] long, with the room, is handed
    /// back as it is; one unit more, and it is verbatim. `C:\…` names no
    /// file here: `GetFullPathNameW` only rewrites the string.
    #[cfg(windows)]
    #[test]
    fn a_path_turns_verbatim_one_unit_past_the_plain_limit() {
        let at = drive_path(PLAIN_PATH_MAX_UNITS - 3);
        assert!(matches!(long_path(&at, 0), Cow::Borrowed(p) if p == at));
        let past = drive_path(PLAIN_PATH_MAX_UNITS - 2);
        let mut want = std::ffi::OsString::from(r"\\?\");
        want.push(past.as_os_str());
        assert_eq!(long_path(&past, 0).as_os_str(), want);
        // The room counts as the path's own length does.
        let short = drive_path(PLAIN_PATH_MAX_UNITS - 3 - 30);
        assert!(matches!(long_path(&short, 30), Cow::Borrowed(_)));
        assert!(long_path(&short, 31)
            .as_os_str()
            .to_string_lossy()
            .starts_with(r"\\?\C:\"));
    }

    /// A short relative path is handed back as written, not made absolute.
    /// What is measured is its absolute form, which starts at the working
    /// folder, so under a checkout deep enough to push that past the limit
    /// the path does turn verbatim, and the test says so and checks nothing.
    #[cfg(windows)]
    #[test]
    fn a_short_relative_path_is_left_as_written() {
        let path = Path::new(r"some\relative.txt");
        let cwd = std::env::current_dir().unwrap();
        // Bytes, never fewer than UTF-16 units: 150 leaves room for the
        // name and the 32 units asked for under `PLAIN_PATH_MAX_UNITS`.
        if cwd.as_os_str().len() > 150 {
            eprintln!("skipped: the working folder is too deep: {}", cwd.display());
            return;
        }
        assert!(matches!(long_path(path, 32), Cow::Borrowed(p) if p == path));
    }

    /// A long path is normalised as Windows normalises one that is not
    /// verbatim, so the verbatim form names what the plain one did: `/`
    /// becomes `\`, `.` and `..` are resolved, a name's trailing dots and
    /// spaces go, and a share's path takes the `UNC` prefix.
    #[cfg(windows)]
    #[test]
    fn a_long_path_is_normalised_before_it_turns_verbatim() {
        let long = "a".repeat(300);
        let cases = [
            (format!("C:/{long}/b.txt"), format!(r"\\?\C:\{long}\b.txt")),
            (
                format!(r"C:\{long}\x\..\.\b.txt"),
                format!(r"\\?\C:\{long}\b.txt"),
            ),
            (
                format!(r"C:\{long}\b.txt. ."),
                format!(r"\\?\C:\{long}\b.txt"),
            ),
            (
                format!(r"\\server\share\{long}\b.txt"),
                format!(r"\\?\UNC\server\share\{long}\b.txt"),
            ),
            (
                format!(r"\\?\C:\{long}\b.txt"),
                format!(r"\\?\C:\{long}\b.txt"),
            ),
        ];
        for (path, want) in cases {
            assert_eq!(long_path(Path::new(&path), 0).as_os_str(), &*want, "{path}");
        }
    }

    /// What `long_path` is for: tempfile's rename refuses a path past
    /// `MAX_PATH` as written, and takes the form `long_path` gives it. The
    /// first half is the bug; if a tempfile release stops passing paths to
    /// Windows as they are, it fails and says so.
    #[cfg(windows)]
    #[test]
    fn tempfile_renames_into_a_long_path_only_in_the_long_form() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("d".repeat(120)).join("e".repeat(120));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("file.txt");
        assert!(target.as_os_str().len() > 260);

        let tmp = tempfile::Builder::new().tempfile_in(&dir).unwrap();
        let refused = tmp.persist(&target).expect_err("a plain long path renamed");
        assert_eq!(refused.error.raw_os_error(), Some(3), "{}", refused.error);
        drop(refused);
        assert!(!target.exists());

        let long = long_path(&target, 32);
        let dir = long.parent().unwrap();
        let mut tmp = tempfile::Builder::new().tempfile_in(dir).unwrap();
        tmp.write_all(b"text").unwrap();
        tmp.persist(&*long).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"text");
    }
}
