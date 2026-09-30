//! The Unix half of [`super`] — Linux and macOS: the key at rest is the
//! file's own bytes, protected by the file's ownership and permissions.
//! Each platform supplies its own randomness and HMAC-SHA256, in the
//! submodule beside this file: the kernel's `getrandom` and `GLib` on
//! Linux, the kernel's `getentropy` and `CommonCrypto` on macOS.
//!
//! There is no account-bound encryption to lean on here — no DPAPI, and
//! a keyring daemon is neither guaranteed to be running nor something an
//! editor should start — so the property this backend keeps is the one a
//! file can: only this account can read or change the key. A key file
//! that some other account owns, or could read or write, is not
//! trusted: whoever could read it could sign a session file for this
//! account, so it is replaced, and nothing it signed checks out. The
//! key file and its temporary are created readable and writable by the
//! owner alone, never looser.
//!
//! macOS's Keychain is the obvious alternative there, and is not used:
//! an item's access list is bound to the signature of the program that
//! made it, so every rebuild of an unsigned `cargo run` binary would
//! prompt to reach the key, and a CI runner has no one to answer.

use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use super::{Blob, KEY_LEN, MAX_KEY_FILE_BYTES};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

pub(super) use os::{hmac_sha256, make_private, random_key};

/// The permission bits a key file may carry: none for the group or for
/// other accounts.
const FOREIGN_ACCESS: u32 = 0o077;

/// Nothing to set up: the HMAC is in a library every process of the
/// backend has loaded, and the randomness is a system call.
#[allow(clippy::unnecessary_wraps)] // one signature for every backend
pub(super) fn ready() -> io::Result<()> {
    Ok(())
}

/// The key as its file holds it: the key itself.
#[allow(clippy::unnecessary_wraps)] // one signature for every backend
pub(super) fn seal(key: &[u8; KEY_LEN]) -> io::Result<Vec<u8>> {
    Ok(key.to_vec())
}

/// The key a file's bytes are, into `key`: exactly [`KEY_LEN`] of them.
pub(super) fn unseal(blob: &[u8], key: &mut [u8; KEY_LEN]) -> io::Result<()> {
    if blob.len() != KEY_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not the length of a key",
        ));
    }
    key.copy_from_slice(blob);
    Ok(())
}

/// The bytes at `path`, at most [`MAX_KEY_FILE_BYTES`] + 1 of them, so
/// an oversized file is recognised without being read whole; `None`
/// when there is no file.
///
/// The last component is opened with `O_NOFOLLOW`, so a symbolic link
/// planted at the path is never followed: the open fails, and the link
/// is reported as something that is not a key — replaced like any other
/// — rather than what it points at being read. `O_NONBLOCK` keeps a
/// FIFO planted there from blocking the open until something writes to
/// it; it is then not a regular file, and is replaced too. A link
/// *above* the key — a whole config directory moved with a symlink — is
/// followed as usual.
///
/// A regular file is read whatever its permissions, and then judged by
/// them: another account's file, or one another account could read or
/// write, cannot hold this account's key. What "could read or write"
/// covers beyond the mode bits is the platform's — see its
/// `foreign_acl`.
pub(super) fn read_capped(path: &Path) -> io::Result<Option<Blob>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Ok(Some(Blob::unusable("a symbolic link")));
        }
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Ok(Some(Blob::unusable("not a regular file")));
    }
    let mut bytes = Vec::new();
    // Through `&File`, so the handle stays for the ACL check below.
    (&file)
        .take(MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    // SAFETY: `geteuid` takes nothing and cannot fail.
    let me = unsafe { libc::geteuid() };
    let unusable = if meta.uid() != me {
        Some("owned by another account")
    } else if meta.mode() & FOREIGN_ACCESS != 0 {
        Some("carries permissions for other accounts")
    } else {
        os::foreign_acl(&file)?
    };
    Ok(Some(Blob {
        bytes,
        unusable: unusable.map(|why| io::Error::new(io::ErrorKind::InvalidData, why)),
    }))
}

/// Try once to take the lock at `lock_path`: `None` while another
/// instance holds it.
///
/// The lock is an exclusive `flock` on the lock file. It belongs to the
/// open file, not the process, so a second open — another instance, or
/// another thread of this one — cannot take it while the first holds
/// it, and the kernel drops it with the file when a process dies, so
/// nothing is ever left locked. `O_NOFOLLOW`: a symbolic link planted at
/// the lock's path fails the attempt rather than being followed, which
/// is failing closed — the key is then not had for this attempt.
/// `O_NONBLOCK`: whatever is at the path, opening it does not wait. A
/// read-write open of a FIFO does not wait on Linux or macOS either, but
/// POSIX leaves that unspecified, and anything but a regular file is
/// refused once it is open (see [`keep_lock_private`]). Nothing is ever
/// written to the file.
pub(super) fn try_lock(lock_path: &Path) -> io::Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)?;
    keep_lock_private(&file)?;
    // SAFETY: `file` owns a valid, open descriptor for the length of the
    // call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EINTR => Ok(None),
        _ => Err(e),
    }
}

/// Refuse a lock that is not a regular file or that another account owns,
/// and close this account's own lock file to every other account.
///
/// Any account that can open the lock file can hold its `flock`, and while
/// it does, every attempt here waits out the lock and gives up: new
/// registrations go unsigned and restored panels are parked. That fails
/// closed, but it would let another account switch the restore off. The
/// file is created owner-only; this puts it back that way if it arrived
/// otherwise — a mode loosened by a copy, or on macOS an ACL inherited
/// from the folder. A lock file another account owns could only have been
/// planted by an account that can write the config folder; it is refused
/// at once, with the reason in the log, rather than waited on.
fn keep_lock_private(file: &std::fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the plugin panel key's lock is not a regular file",
        ));
    }
    // SAFETY: `geteuid` takes nothing and cannot fail.
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the plugin panel key's lock file is owned by another account",
        ));
    }
    if meta.mode() & FOREIGN_ACCESS != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    make_private(file)
}

#[cfg(test)]
pub(super) mod test_support {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// Puts a file's permissions back when dropped.
    pub(in crate::panel_key) struct Unreadable {
        path: PathBuf,
        mode: u32,
    }

    impl Drop for Unreadable {
        fn drop(&mut self) {
            let _ =
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.mode));
        }
    }

    /// Make `path` unreadable to this account until the guard drops —
    /// or `None` when this account reads any file regardless (root, or
    /// on Linux a process holding `CAP_DAC_OVERRIDE`), measured rather
    /// than guessed.
    ///
    /// Locally that skips the test. Under CI it fails instead: `cargo
    /// test` hides a passing test's output, so a skip there would drop
    /// the coverage while the run stays green — the trap DEVELOPMENT.md
    /// §2.6 records for Windows' symlink tests. A CI runner has to run
    /// the Linux and macOS tests as an ordinary account (§3.4, §4.5).
    pub(in crate::panel_key) fn make_unreadable(path: &Path) -> Option<Unreadable> {
        let mode = std::fs::metadata(path).expect("stat").permissions().mode();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let guard = Unreadable {
            path: path.to_path_buf(),
            mode,
        };
        if std::fs::File::open(path).is_ok() {
            assert!(
                std::env::var_os("CI").is_none(),
                "this account reads a file it has no permission to read (root?), so the \
                 unreadable-key tests cannot run: CI must run the Linux and macOS tests \
                 as an ordinary account (DEVELOPMENT.md §3.4, §4.5)"
            );
            return None;
        }
        Some(guard)
    }

    /// Hold the key's lock for good, the way any process running as the
    /// user can: an exclusive `flock` on the lock file, from an open of
    /// its own.
    pub(in crate::panel_key) fn hold_lock(lock_path: &Path) -> std::fs::File {
        let file = std::fs::File::open(lock_path).expect("hold");
        // SAFETY: `file` owns a valid, open descriptor.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(locked, 0, "the lock could not be held");
        file
    }
}

#[cfg(test)]
mod tests {
    use super::super::PanelKey;
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    /// The file holds the key itself, and no account but this one may
    /// read or change it — which is all that protects it on this
    /// platform, so it is pinned.
    #[test]
    fn the_key_file_is_the_bare_key_for_this_account_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let key = PanelKey::load_or_create(&path).expect("create");
        assert_eq!(std::fs::read(&path).expect("read"), key.key);
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(
            mode_of(&dir.path().join("panel-restore.key.lock")),
            0o600,
            "and so is the lock beside it"
        );
    }

    /// A key other accounts could read is no key at all: any of them
    /// could have signed a session file with it. Replaced, and the
    /// replacement is this account's alone.
    #[test]
    fn a_key_file_other_accounts_could_read_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let exposed = [5u8; KEY_LEN];
        std::fs::write(&path, exposed).expect("plant");
        for mode in [0o644, 0o620, 0o604] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
            let key = PanelKey::load_or_create(&path).expect("replaced");
            assert_ne!(key.key, exposed, "a {mode:o} key was trusted");
            assert_eq!(mode_of(&path), 0o600, "replaced by a private file");
            std::fs::write(&path, exposed).expect("plant again");
        }
    }

    /// A lock file other accounts could open is closed to them before it
    /// is locked: an account able to open it could hold the lock, and
    /// every attempt here would then give up. It stays the same file —
    /// nothing is written to a lock, so there is nothing to replace.
    #[test]
    fn a_lock_file_other_accounts_could_open_is_closed_to_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = dir.path().join("panel-restore.key.lock");
        for mode in [0o644, 0o660, 0o606] {
            std::fs::write(&lock, b"").expect("plant");
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(mode)).expect("chmod");
            let held = try_lock(&lock).expect("lock").expect("not held elsewhere");
            assert_eq!(mode_of(&lock), 0o600, "a {mode:o} lock file was left open");
            drop(held);
        }
    }

    /// A file of the wrong length is not a key, whatever its
    /// permissions.
    #[test]
    fn a_key_of_the_wrong_length_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        for planted in [vec![1u8; KEY_LEN - 1], vec![2u8; KEY_LEN + 1]] {
            std::fs::write(&path, &planted).expect("plant");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
            let key = PanelKey::load_or_create(&path).expect("replaced");
            assert_eq!(std::fs::read(&path).expect("read"), key.key);
        }
    }

    /// A symbolic link planted at the key's path is not followed. Its
    /// target holds a real key for this account, so following it would
    /// *succeed* — only not following it tells the two apart. What it
    /// points at is neither read as the key nor written, and the link is
    /// replaced by a key of its own. Creating a symlink needs no
    /// privilege here, so this runs everywhere, unlike its Windows twin.
    #[test]
    fn a_symlink_at_the_keys_path_is_not_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let target = dir.path().join("elsewhere.bin");
        let planted = [9u8; KEY_LEN];
        std::fs::write(&target, planted).expect("plant");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
        let key = PanelKey::load_or_create(&path).expect("created");
        assert_ne!(key.key, planted, "the link's target was read as the key");
        assert_eq!(
            std::fs::read(&target).expect("read"),
            planted,
            "and was not written"
        );
        assert!(
            !std::fs::symlink_metadata(&path)
                .expect("stat")
                .file_type()
                .is_symlink(),
            "the link is replaced by a key"
        );
    }

    /// A FIFO planted at the key's path does not hang the UI thread
    /// waiting for a writer that never comes: it is not a regular file,
    /// so it is replaced — promptly.
    #[test]
    fn a_fifo_at_the_keys_path_does_not_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: `c_path` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
        let started = Instant::now();
        let key = PanelKey::load_or_create(&path).expect("replaced");
        assert!(started.elapsed() < Duration::from_secs(5), "it blocked");
        assert_eq!(std::fs::read(&path).expect("read"), key.key);
    }

    /// A FIFO planted at the lock's path is refused, and opening it does
    /// not wait for a writer on the way.
    #[test]
    fn a_fifo_at_the_locks_path_fails_closed_without_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = dir.path().join("panel-restore.key.lock");
        let c_path = std::ffi::CString::new(lock.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: `c_path` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
        let started = Instant::now();
        let refused = try_lock(&lock).expect_err("a FIFO was taken as the lock");
        assert!(started.elapsed() < Duration::from_secs(5), "it blocked");
        // Refused for what it is, not for whatever a later call happens to
        // make of a FIFO: macOS's `flock` refuses one too (`ENOTSUP`),
        // which would hide a missing check here. The check is what refuses
        // it wherever the lock call would not.
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "{refused}");
    }

    /// A symbolic link planted at the lock's path makes the attempt fail
    /// rather than lock — or create — whatever it points at.
    #[test]
    fn a_symlink_at_the_locks_path_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let target = dir.path().join("not-created-by-the-lock");
        std::os::unix::fs::symlink(&target, dir.path().join("panel-restore.key.lock"))
            .expect("symlink");
        assert!(PanelKey::load_or_create(&path).is_err());
        assert!(!target.exists(), "the lock created the link's target");
        assert!(!path.exists(), "and nothing was written without the lock");
    }
}
