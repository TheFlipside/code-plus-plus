//! macOS's primitives for [`super`]: randomness from the kernel's
//! `getentropy`, HMAC-SHA256 from `CommonCrypto`, and the one protection
//! the mode bits cannot vouch for here — extended ACLs.
//!
//! `CommonCrypto` is the platform's own primitive in the sense CNG is
//! Windows' and `GLib` is the GTK desktop's: it is part of `libSystem`,
//! which every process links, so no framework and no crate is added.
//!
//! # Why ACLs are checked here and not on Linux
//!
//! A macOS file can carry an extended ACL (`chmod +a`) whose entries
//! grant other accounts access **without showing in the mode bits** —
//! unlike a POSIX ACL, whose mask is the group bits. So a key file that
//! passes the owner and `0600` checks can still be readable by another
//! account, and one is treated as not a key. The same entries are
//! inherited: a new file in a directory whose ACL carries `file_inherit`
//! entries gets them, whatever mode it is created with. A key created
//! there would be judged unusable at the next read and replaced on every
//! start, so the key's temporary file has its inherited entries removed
//! before it is renamed into place ([`make_private`]).

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

use core::ffi::{c_int, c_void};

use super::super::KEY_LEN;

/// `kCCHmacAlgSHA256`, from `<CommonCrypto/CommonHMAC.h>`: the third
/// member of its algorithm enum, after SHA-1 and MD5.
const CC_HMAC_ALG_SHA256: u32 = 2;

/// `ACL_TYPE_EXTENDED`, from `<sys/acl.h>`: the only ACL type macOS
/// supports.
const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
/// `ACL_EXTENDED_ALLOW`, the tag of an entry that grants access.
const ACL_EXTENDED_ALLOW: c_int = 1;
/// `ACL_FIRST_ENTRY` / `ACL_NEXT_ENTRY`, for walking an ACL's entries.
const ACL_FIRST_ENTRY: c_int = 0;
const ACL_NEXT_ENTRY: c_int = -1;

// All in `libSystem`: `CommonCrypto` and the ACL calls both. Declared
// here because the `libc` crate carries neither.
extern "C" {
    fn CCHmac(
        algorithm: u32,
        key: *const c_void,
        key_length: usize,
        data: *const c_void,
        data_length: usize,
        mac_out: *mut c_void,
    );
    fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> *mut c_void;
    fn acl_set_fd_np(fd: c_int, acl: *mut c_void, acl_type: c_int) -> c_int;
    fn acl_init(count: c_int) -> *mut c_void;
    fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
    fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
    fn acl_free(obj: *mut c_void) -> c_int;
}

/// 32 bytes from the kernel's random number generator.
///
/// `getentropy` is the macOS system call for exactly this — seeding
/// material, at most 256 bytes per call — and it cannot return fewer
/// bytes than asked for: it fills the buffer or fails.
pub(in crate::panel_key) fn random_key() -> io::Result<[u8; KEY_LEN]> {
    let mut key = [0u8; KEY_LEN];
    // SAFETY: `key` is a valid, writable buffer of `key.len()` bytes,
    // well under `getentropy`'s 256-byte limit.
    if unsafe { libc::getentropy(key.as_mut_ptr().cast(), key.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(key)
}

/// HMAC-SHA256 of `message` under `key`, through `CommonCrypto`'s
/// one-shot `CCHmac`, which cannot fail: `Some` always, for the
/// signature every platform shares.
#[allow(clippy::unnecessary_wraps)] // one signature for every platform
pub(in crate::panel_key) fn hmac_sha256(key: &[u8], message: &[u8]) -> Option<[u8; KEY_LEN]> {
    let mut out = [0u8; KEY_LEN];
    // SAFETY: `key` and `message` are valid buffers of the lengths
    // passed, and `out` holds exactly the 32 bytes a SHA-256 MAC is.
    unsafe {
        CCHmac(
            CC_HMAC_ALG_SHA256,
            key.as_ptr().cast(),
            key.len(),
            message.as_ptr().cast(),
            message.len(),
            out.as_mut_ptr().cast(),
        );
    }
    Some(out)
}

/// Whether `err` says the file system keeps no ACLs at all — FAT and
/// exFAT volumes, some network shares. A file there has no ACL to grant
/// anything, or to strip.
fn acls_unsupported(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::ENOTSUP | libc::EOPNOTSUPP))
}

/// Remove every extended-ACL entry from `file`: the entries a new file
/// inherits from its directory, which on this platform can grant other
/// accounts access the mode bits do not show. Called on the key's
/// temporary file before it is renamed into place. A file system that
/// keeps no ACLs has nothing to remove.
pub(in crate::panel_key) fn make_private(file: &File) -> io::Result<()> {
    // SAFETY: `acl_init` takes a count and returns a new, empty ACL or
    // null; the ACL is freed on every path once it has been used.
    let empty = unsafe { acl_init(0) };
    if empty.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `file` owns a valid, open descriptor for the length of the
    // call, and `empty` is the live ACL just made.
    let set = unsafe { acl_set_fd_np(file.as_raw_fd(), empty, ACL_TYPE_EXTENDED) };
    let err = (set != 0).then(io::Error::last_os_error);
    // SAFETY: `empty` came from `acl_init` and is freed exactly once.
    unsafe { acl_free(empty) };
    match err {
        Some(e) if !acls_unsupported(&e) => Err(e),
        _ => Ok(()),
    }
}

/// Why `file`'s extended ACL rules it out as a key, or `None` when it
/// has none that grants anything.
///
/// Any *allow* entry does: an entry naming the owner grants nothing
/// they lack, but telling the owner's entries from anyone else's means
/// resolving each qualifier through Directory Services, and a key file
/// Code++ made never carries one at all — [`make_private`] sees to that.
/// *Deny* entries only take access away, and are left alone: the
/// `group:everyone deny delete` entry macOS puts on a home folder is
/// the common one. An entry whose tag cannot be read is taken to allow.
pub(in crate::panel_key) fn foreign_acl(file: &File) -> io::Result<Option<&'static str>> {
    // SAFETY: `file` owns a valid, open descriptor for the length of the
    // call. The ACL returned is freed below on every path.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let e = io::Error::last_os_error();
        // `ENOENT` is how the call says the file has no ACL.
        if e.raw_os_error() == Some(libc::ENOENT) || acls_unsupported(&e) {
            return Ok(None);
        }
        return Err(e);
    }
    let mut grants = false;
    let mut entry: *mut c_void = std::ptr::null_mut();
    let mut which = ACL_FIRST_ENTRY;
    // SAFETY: `acl` is the live ACL fetched above; `entry` receives a
    // pointer into it, valid until the ACL is freed, and is read only
    // before that. The walk ends when `acl_get_entry` reports no more.
    unsafe {
        while acl_get_entry(acl, which, &raw mut entry) == 0 {
            let mut tag: c_int = 0;
            if acl_get_tag_type(entry, &raw mut tag) != 0 || tag == ACL_EXTENDED_ALLOW {
                grants = true;
                break;
            }
            which = ACL_NEXT_ENTRY;
        }
        acl_free(acl);
    }
    Ok(grants.then_some("an access control list grants access to another account"))
}

#[cfg(test)]
mod tests {
    use super::super::super::PanelKey;
    use std::path::Path;
    use std::process::Command;

    /// `chmod` with `args`, through the system's own tool: the plain way
    /// to put an extended ACL on a file, and the one a user would use.
    fn chmod(args: &[&str], path: &Path) {
        let status = Command::new("/bin/chmod")
            .args(args)
            .arg(path)
            .status()
            .expect("run chmod");
        assert!(status.success(), "chmod {args:?} failed");
    }

    /// Whether `path` carries any extended-ACL entry, as `ls -le` shows
    /// them: one numbered line per entry after the file's own.
    fn has_acl(path: &Path) -> bool {
        let out = Command::new("/bin/ls")
            .arg("-led")
            .arg(path)
            .output()
            .expect("run ls");
        String::from_utf8_lossy(&out.stdout).lines().count() > 1
    }

    /// A key another account's ACL entry can read is no key at all,
    /// though its mode bits say `0600`: it is replaced, and the
    /// replacement carries no ACL.
    #[test]
    fn a_key_file_an_acl_shares_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let original = PanelKey::load_or_create(&path).expect("create");
        chmod(&["+a", "everyone allow read"], &path);
        assert!(has_acl(&path), "the fixture ACL is not on the file");
        let key = PanelKey::load_or_create(&path).expect("replaced");
        assert_ne!(key.key, original.key, "an ACL-shared key was trusted");
        assert!(!has_acl(&path), "the replacement carries the ACL");
        assert_eq!(
            PanelKey::load_or_create(&path).expect("read").key,
            key.key,
            "and it is kept from then on"
        );
    }

    /// A deny entry takes nothing away from the owner's privacy, so a key
    /// carrying one is kept.
    #[test]
    fn a_deny_only_acl_does_not_disqualify_the_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let original = PanelKey::load_or_create(&path).expect("create");
        chmod(&["+a", "everyone deny delete"], &path);
        assert_eq!(
            PanelKey::load_or_create(&path).expect("read").key,
            original.key
        );
    }

    /// A key created in a directory whose ACL hands `file_inherit`
    /// entries down arrives without them, so it is kept at the next read
    /// rather than replaced on every start.
    #[test]
    fn a_key_created_under_an_inheriting_acl_is_private_and_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        chmod(&["+a", "everyone allow read,file_inherit"], dir.path());
        let path = dir.path().join("panel-restore.key");
        let key = PanelKey::load_or_create(&path).expect("create");
        assert!(!has_acl(&path), "the key kept the inherited entries");
        assert_eq!(PanelKey::load_or_create(&path).expect("read").key, key.key);
    }

    /// The lock beside the key is kept free of ACL entries too — ones
    /// inherited from the folder, or put on it afterwards — because an
    /// account that can open the lock can hold it, and every attempt to
    /// have the key then gives up.
    #[test]
    fn the_lock_file_is_kept_free_of_acl_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        chmod(&["+a", "everyone allow read,file_inherit"], dir.path());
        let path = dir.path().join("panel-restore.key");
        let lock = dir.path().join("panel-restore.key.lock");
        PanelKey::load_or_create(&path).expect("create");
        assert!(!has_acl(&lock), "the lock kept the inherited entries");
        chmod(&["+a", "everyone allow read"], &lock);
        assert!(has_acl(&lock), "the fixture ACL is not on the lock");
        PanelKey::load_or_create(&path).expect("read");
        assert!(!has_acl(&lock), "the lock's ACL was left in place");
    }
}
