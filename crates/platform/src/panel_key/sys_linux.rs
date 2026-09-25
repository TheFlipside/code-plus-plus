//! The Linux half of [`super`]: the key at rest is the file's own bytes,
//! protected by the file's ownership and permissions; its randomness
//! comes from the kernel's `getrandom`, and the HMAC-SHA256 from `GLib`.
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
//! `GLib` rather than a crypto crate because it is the platform's own
//! primitive in the sense CNG is Windows': it is already mapped into
//! every process of this backend (GTK is built on it), its HMAC is
//! RFC 2104 over its SHA-256, and a test pins the result against
//! RFC 4231's vectors.

use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use super::{Blob, KEY_LEN, MAX_KEY_FILE_BYTES};

/// The permission bits a key file may carry: none for the group or for
/// other accounts.
const FOREIGN_ACCESS: u32 = 0o077;

/// Nothing to set up: `GLib` is linked, and `getrandom` is a system
/// call.
#[allow(clippy::unnecessary_wraps)] // one signature for both backends
pub(super) fn ready() -> io::Result<()> {
    Ok(())
}

/// The key as its file holds it: the key itself.
#[allow(clippy::unnecessary_wraps)] // one signature for both backends
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

/// 32 bytes from the kernel's random number generator.
///
/// `getrandom` with no flags draws from the same pool as `/dev/urandom`
/// and blocks only until that pool has been seeded once, early in boot —
/// never on an editor's timescale.
pub(super) fn random_key() -> io::Result<[u8; KEY_LEN]> {
    let mut key = [0u8; KEY_LEN];
    let mut filled = 0;
    while filled < KEY_LEN {
        let rest = &mut key[filled..];
        // SAFETY: `rest` is a valid, writable buffer of `rest.len()`
        // bytes, and `getrandom` writes at most that many.
        let n = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        if let Ok(written) = usize::try_from(n) {
            if written == 0 {
                // Not something the kernel does for a request this
                // small, but looping on it would spin rather than fail.
                return Err(io::Error::other("getrandom returned no bytes"));
            }
            filled += written;
        } else {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
    Ok(key)
}

/// HMAC-SHA256 of `message` under `key`, through `GLib`'s `GHmac`.
pub(super) fn hmac_sha256(key: &[u8], message: &[u8]) -> Option<[u8; KEY_LEN]> {
    let message_len = isize::try_from(message.len()).ok()?;
    // SAFETY: `key` and `message` are valid buffers of the lengths
    // passed; `GLib` copies what it needs from `key` before `g_hmac_new`
    // returns. The handle is checked for null, used once, and released
    // on every path after it is made.
    unsafe {
        let hmac = glib_sys::g_hmac_new(glib_sys::G_CHECKSUM_SHA256, key.as_ptr(), key.len());
        if hmac.is_null() {
            return None;
        }
        glib_sys::g_hmac_update(hmac, message.as_ptr(), message_len);
        let mut out = [0u8; KEY_LEN];
        let mut out_len = out.len();
        glib_sys::g_hmac_get_digest(hmac, out.as_mut_ptr(), &raw mut out_len);
        glib_sys::g_hmac_unref(hmac);
        (out_len == KEY_LEN).then_some(out)
    }
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
/// write, cannot hold this account's key.
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
    file.take(MAX_KEY_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    // SAFETY: `geteuid` takes nothing and cannot fail.
    let me = unsafe { libc::geteuid() };
    // The group and other bits cover POSIX ACLs too: on a file with an
    // extended ACL the group bits *are* the ACL's mask, which bounds
    // every named user and group entry, so with them clear no other
    // account gets anything, whatever the ACL lists. NFSv4 ACLs do not
    // work that way — a named-user entry need not show in the mode — and
    // are not read; a key on such a share is only as private as its ACL.
    let unusable = if meta.uid() != me {
        Some("owned by another account")
    } else if meta.mode() & FOREIGN_ACCESS != 0 {
        Some("carries permissions for other accounts")
    } else {
        None
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
/// is failing closed — the key is then not had for this attempt. Nothing
/// is ever written to the file.
pub(super) fn try_lock(lock_path: &Path) -> io::Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_path)?;
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
    /// a process holding `CAP_DAC_OVERRIDE`), measured rather than
    /// guessed.
    ///
    /// Locally that skips the test. Under CI it fails instead: `cargo
    /// test` hides a passing test's output, so a skip there would drop
    /// the coverage while the run stays green — the trap DEVELOPMENT.md
    /// §2.6 records for Windows' symlink tests. A CI runner has to run
    /// the Linux tests as an ordinary account (§3.4).
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
                 unreadable-key tests cannot run: CI must run the Linux tests as an \
                 ordinary account (DEVELOPMENT.md §3.4)"
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
