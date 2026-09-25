//! Linux's primitives for [`super`]: randomness from the kernel's
//! `getrandom`, HMAC-SHA256 from `GLib`.
//!
//! `GLib` rather than a crypto crate because it is the platform's own
//! primitive in the sense CNG is Windows': it is already mapped into
//! every process of the GTK backend (GTK is built on it), its HMAC is
//! RFC 2104 over its SHA-256, and a test pins the result against
//! RFC 4231's vectors.

use std::fs::File;
use std::io;

use super::super::KEY_LEN;

/// 32 bytes from the kernel's random number generator.
///
/// `getrandom` with no flags draws from the same pool as `/dev/urandom`
/// and blocks only until that pool has been seeded once, early in boot —
/// never on an editor's timescale.
pub(in crate::panel_key) fn random_key() -> io::Result<[u8; KEY_LEN]> {
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
pub(in crate::panel_key) fn hmac_sha256(key: &[u8], message: &[u8]) -> Option<[u8; KEY_LEN]> {
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

/// Nothing to strip. A directory's default POSIX ACL does reach a new
/// file, but masked by the group bits of the mode the file is created
/// with — and a staged key is created `0600`, so its mask grants no
/// named user or group anything.
#[allow(clippy::unnecessary_wraps)] // one signature for every platform
pub(in crate::panel_key) fn make_private(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Always `None`. The mode bits the caller has already checked cover
/// POSIX ACLs: on a file with an extended ACL the group bits *are* the
/// ACL's mask, which bounds every named user and group entry, so with
/// them clear no other account gets anything, whatever the ACL lists.
/// `NFSv4` ACLs do not work that way — a named-user entry need not show in
/// the mode — and are not read; a key on such a share is only as private
/// as its ACL.
#[allow(clippy::unnecessary_wraps)] // one signature for every platform
pub(in crate::panel_key) fn foreign_acl(_file: &File) -> io::Result<Option<&'static str>> {
    Ok(None)
}
