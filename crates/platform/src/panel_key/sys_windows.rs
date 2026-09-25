//! The Windows half of [`super`]: the key at rest encrypted with DPAPI
//! for the current account, and its randomness and the HMAC-SHA256 from
//! Windows' own CNG provider.
//!
//! Both libraries are loaded the first time a key is needed rather than
//! imported — see [`Crypto`] — so a session that never signs or checks
//! anything starts exactly as it did before the key existed.

use std::ffi::c_void;
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::OnceLock;

use windows::core::{s, w};
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    BCRYPT_HMAC_SHA256_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG, CRYPTPROTECT_UI_FORBIDDEN,
    CRYPT_INTEGER_BLOB,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

use super::{Blob, KEY_LEN, MAX_KEY_FILE_BYTES};

/// DPAPI's optional entropy for the key file: a second input the file
/// needs to decrypt, so a blob some other program protected for the same
/// account is not mistaken for this key, and this one is not read by a
/// program that simply asks DPAPI to decrypt whatever it finds.
const ENTROPY: &[u8] = b"Code++ plugin panel key v1";

/// `CreateFileW`'s `FILE_FLAG_OPEN_REPARSE_POINT`: open a link itself
/// rather than what it points at. The same bare constant
/// `codepp_shell::fif` uses, fixed since Windows 2000.
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// `ERROR_SHARING_VIOLATION`: another handle holds the file with no
/// sharing — here, another instance holding the key's lock.
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Whether DPAPI and CNG are there to use. Without them, a file that
/// does not decrypt says nothing about the file.
pub(super) fn ready() -> io::Result<()> {
    crypto().map(|_| ())
}

/// The key as its file holds it: a DPAPI blob for this account.
pub(super) fn seal(key: &[u8; KEY_LEN]) -> io::Result<Vec<u8>> {
    protect(key)
}

/// The key a file's DPAPI blob decrypts to, into `key`.
pub(super) fn unseal(blob: &[u8], key: &mut [u8; KEY_LEN]) -> io::Result<()> {
    unprotect_into(blob, key)
}

/// Nothing to remove. The file's ACL is inherited from the profile as
/// every file there is, and whatever it grants another account, the
/// file holds a DPAPI blob only this account can decrypt.
#[allow(clippy::unnecessary_wraps)] // one signature for every backend
pub(super) fn make_private(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
}

/// The bytes at `path`, at most [`MAX_KEY_FILE_BYTES`] + 1 of them, so
/// an oversized file is recognised without being read whole; `None`
/// when there is no file.
///
/// A link planted at the path is not followed: the handle is to the
/// link itself, which is not a regular file and so is never read, and
/// the link is replaced like any other file that is not a key. A link
/// *above* the key — a whole config directory moved with a junction —
/// is followed as usual; only the last component is opened this way.
pub(super) fn read_capped(path: &Path) -> io::Result<Option<Blob>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Ok(Some(Blob::unusable("not a regular file")));
    }
    let mut bytes = Vec::new();
    file.take(MAX_KEY_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    Ok(Some(Blob {
        bytes,
        unusable: None,
    }))
}

/// Try once to take the lock at `lock_path`: `None` while another
/// instance holds it.
///
/// The lock file is opened with no sharing: a second instance's open
/// fails with a sharing violation for as long as the first holds it.
/// Holding the handle is the lock, and the system closes a dying
/// process's handles, so nothing is ever left locked. Like the key, a
/// link planted at the lock's path is opened itself rather than
/// followed, and nothing is ever written to it.
pub(super) fn try_lock(lock_path: &Path) -> io::Result<Option<std::fs::File>> {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(lock_path)
    {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
        Err(e) => Err(e),
    }
}

type BCryptGenRandomFn = unsafe extern "system" fn(*mut c_void, *mut u8, u32, u32) -> i32;
type BCryptHashFn =
    unsafe extern "system" fn(*mut c_void, *const u8, u32, *const u8, u32, *mut u8, u32) -> i32;
type CryptProtectDataFn = unsafe extern "system" fn(
    *const CRYPT_INTEGER_BLOB,
    *const u16,
    *const CRYPT_INTEGER_BLOB,
    *const c_void,
    *const c_void,
    u32,
    *mut CRYPT_INTEGER_BLOB,
) -> i32;
type CryptUnprotectDataFn = unsafe extern "system" fn(
    *const CRYPT_INTEGER_BLOB,
    *mut *mut u16,
    *const CRYPT_INTEGER_BLOB,
    *const c_void,
    *const c_void,
    u32,
    *mut CRYPT_INTEGER_BLOB,
) -> i32;

/// The four CNG and DPAPI entry points this module calls, found in
/// `bcrypt.dll` and `crypt32.dll` the first time a key is needed.
///
/// Imported the ordinary way, they put both libraries in the
/// executable's import table, and Windows then maps them at every
/// start: measured, neither is imported without this module, and a
/// release build importing them started about 2 ms slower (median of
/// ten alternating runs each). A session with no plugin panel never
/// signs or checks anything, so it should not pay that. The libraries
/// are loaded from System32 only, so a DLL of the same name beside
/// `codepp.exe` or in the current directory is never picked up, and are
/// never unloaded, as for any library the process goes on using.
struct Crypto {
    gen_random: BCryptGenRandomFn,
    hash: BCryptHashFn,
    protect: CryptProtectDataFn,
    unprotect: CryptUnprotectDataFn,
}

/// [`Crypto`], resolved once for the process; an error if a library or
/// an entry point could not be found, which leaves nothing signed.
fn crypto() -> io::Result<&'static Crypto> {
    static CRYPTO: OnceLock<Option<Crypto>> = OnceLock::new();
    CRYPTO
        .get_or_init(|| {
            let found = resolve_crypto();
            if found.is_none() {
                tracing::warn!(
                    "CNG or DPAPI is not available; plugin panel commands cannot be signed"
                );
            }
            found
        })
        .as_ref()
        .ok_or_else(|| io::Error::other("CNG or DPAPI is not available"))
}

fn resolve_crypto() -> Option<Crypto> {
    type Farproc = unsafe extern "system" fn() -> isize;
    // SAFETY: `LOAD_LIBRARY_SEARCH_SYSTEM32` confines both loads to the
    // system directory. Each entry point is transmuted, from the
    // function pointer `GetProcAddress` returns, to the signature its
    // SDK header declares — the same size, and the only use made of it.
    unsafe {
        let bcrypt = LoadLibraryExW(w!("bcrypt.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
        let crypt32 = LoadLibraryExW(w!("crypt32.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
        Some(Crypto {
            gen_random: std::mem::transmute::<Farproc, BCryptGenRandomFn>(GetProcAddress(
                bcrypt,
                s!("BCryptGenRandom"),
            )?),
            hash: std::mem::transmute::<Farproc, BCryptHashFn>(GetProcAddress(
                bcrypt,
                s!("BCryptHash"),
            )?),
            protect: std::mem::transmute::<Farproc, CryptProtectDataFn>(GetProcAddress(
                crypt32,
                s!("CryptProtectData"),
            )?),
            unprotect: std::mem::transmute::<Farproc, CryptUnprotectDataFn>(GetProcAddress(
                crypt32,
                s!("CryptUnprotectData"),
            )?),
        })
    }
}

/// `bytes.len()` as the `u32` a Windows API takes, or an error for a
/// buffer too large to describe — never a truncated length.
fn len_u32(bytes: &[u8]) -> io::Result<u32> {
    u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "buffer too large"))
}

/// 32 bytes from the system's preferred random number generator.
pub(super) fn random_key() -> io::Result<[u8; KEY_LEN]> {
    let crypto = crypto()?;
    let mut key = [0u8; KEY_LEN];
    let len = len_u32(&key)?;
    // SAFETY: `key` is a valid, writable buffer of the length passed; a
    // null algorithm handle with this flag asks for the system's
    // preferred generator.
    let status = unsafe {
        (crypto.gen_random)(
            core::ptr::null_mut(),
            key.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG.0,
        )
    };
    if status >= 0 {
        Ok(key)
    } else {
        Err(io::Error::other(format!(
            "BCryptGenRandom failed: {status:#x}"
        )))
    }
}

/// HMAC-SHA256 of `message` under `key`, through CNG's HMAC-SHA256
/// pseudo-handle (Windows 10 and later).
pub(super) fn hmac_sha256(key: &[u8], message: &[u8]) -> Option<[u8; KEY_LEN]> {
    let crypto = crypto().ok()?;
    let mut out = [0u8; KEY_LEN];
    let (key_len, message_len, out_len) = (
        len_u32(key).ok()?,
        len_u32(message).ok()?,
        len_u32(&out).ok()?,
    );
    // SAFETY: the pseudo-handle needs no opening or closing; `key`,
    // `message` and `out` are valid buffers of the lengths passed.
    let status = unsafe {
        (crypto.hash)(
            BCRYPT_HMAC_SHA256_ALG_HANDLE.0,
            key.as_ptr(),
            key_len,
            message.as_ptr(),
            message_len,
            out.as_mut_ptr(),
            out_len,
        )
    };
    (status >= 0).then_some(out)
}

/// A DPAPI blob of `plain` for the current account, with [`ENTROPY`].
fn protect(plain: &[u8]) -> io::Result<Vec<u8>> {
    protect_with(plain, Some(ENTROPY))
}

/// A DPAPI blob of `plain` for the current account, with `entropy`.
fn protect_with(plain: &[u8], entropy: Option<&[u8]>) -> io::Result<Vec<u8>> {
    let crypto = crypto()?;
    let input = blob_of(plain)?;
    let entropy = entropy.map(blob_of).transpose()?;
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: the input blobs point at live buffers of the lengths they
    // carry, and DPAPI only reads them; `out` receives a buffer DPAPI
    // allocates, which is copied and then freed with `LocalFree` below.
    let ok = unsafe {
        (crypto.protect)(
            &raw const input,
            w!("Code++ plugin panel key").as_ptr(),
            entropy
                .as_ref()
                .map_or(core::ptr::null(), core::ptr::from_ref),
            core::ptr::null(),
            core::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut out,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `out` describes a buffer of `cbData` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    // SAFETY: `out.pbData` was allocated by DPAPI with `LocalAlloc`.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
    }
    Ok(bytes)
}

/// Decrypt the DPAPI blob `blob` into `key`, which it must fill exactly.
fn unprotect_into(blob: &[u8], key: &mut [u8; KEY_LEN]) -> io::Result<()> {
    let crypto = crypto()?;
    let input = blob_of(blob)?;
    let entropy = blob_of(ENTROPY)?;
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect_with`.
    let ok = unsafe {
        (crypto.unprotect)(
            &raw const input,
            core::ptr::null_mut(),
            &raw const entropy,
            core::ptr::null(),
            core::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut out,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = out.cbData as usize;
    let result = if len == KEY_LEN {
        // SAFETY: on success `out` describes a buffer of `len` bytes.
        key.copy_from_slice(unsafe { std::slice::from_raw_parts(out.pbData, len) });
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "decrypts, but not to a key",
        ))
    };
    // SAFETY: `out.pbData` is DPAPI's buffer of `len` bytes; it held the
    // key in the clear, so it is wiped before it goes back to the heap.
    unsafe {
        for i in 0..len {
            std::ptr::write_volatile(out.pbData.add(i), 0);
        }
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
    }
    result
}

/// A DPAPI blob descriptor over `bytes`. DPAPI takes a mutable pointer
/// but only reads an input blob.
fn blob_of(bytes: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
    Ok(CRYPT_INTEGER_BLOB {
        cbData: len_u32(bytes)?,
        pbData: bytes.as_ptr().cast_mut(),
    })
}

#[cfg(test)]
pub(super) mod test_support {
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    /// Hold `path` open with no sharing, so every other open fails — a
    /// scanner's momentary lock, reproducibly. Always possible here, so
    /// never `None`; the `Option` is the shared tests' signature, which
    /// the Linux backend needs for an account that reads any file.
    #[allow(clippy::unnecessary_wraps)]
    pub(in crate::panel_key) fn make_unreadable(path: &Path) -> Option<std::fs::File> {
        Some(
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(path)
                .expect("lock"),
        )
    }

    /// Hold the key's lock for good, the way any process running as the
    /// user can: any handle open on the lock file makes the lock's own
    /// no-sharing open fail.
    pub(in crate::panel_key) fn hold_lock(lock_path: &Path) -> std::fs::File {
        std::fs::File::open(lock_path).expect("hold")
    }
}

#[cfg(test)]
mod tests {
    use super::super::PanelKey;
    use super::*;

    /// What is on disk is a DPAPI blob, not the key.
    #[test]
    fn a_created_key_is_not_stored_in_the_clear() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let key = PanelKey::load_or_create(&path).expect("create");
        let blob = std::fs::read(&path).expect("written");
        assert!(blob.len() > KEY_LEN, "a DPAPI blob, not the bare key");
        assert!(
            !blob.windows(KEY_LEN).any(|w| w == key.key),
            "the key is not in the file in the clear"
        );
    }

    /// A real DPAPI blob made for another purpose — one protected
    /// without this key's entropy — decrypts for this account and is
    /// still not taken for the key.
    #[test]
    fn a_dpapi_blob_made_for_another_purpose_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let secret = [7u8; KEY_LEN];
        let foreign = protect_with(&secret, None).expect("protect");
        std::fs::write(&path, &foreign).expect("plant");
        let key = PanelKey::load_or_create(&path).expect("replaced");
        assert_ne!(key.key, secret);
        assert_ne!(std::fs::read(&path).expect("read"), foreign, "replaced");
    }

    /// A symbolic link planted at the key's path is not followed. Its
    /// target holds a real key for this account, so following it would
    /// *succeed* — only not following it tells the two apart. What it
    /// points at is neither read as the key nor written, and the link is
    /// replaced by a key of its own.
    ///
    /// Creating a symlink needs `SeCreateSymbolicLinkPrivilege`
    /// (Developer Mode), so this is ignored by default and run with
    /// `--include-ignored`, as the find-in-files link tests are
    /// (DEVELOPMENT.md §2.6).
    #[test]
    #[ignore = "creating a symlink needs Developer Mode"]
    fn a_symlink_at_the_keys_path_is_not_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let target = dir.path().join("elsewhere.bin");
        let planted = [9u8; KEY_LEN];
        let blob = protect(&planted).expect("protect");
        std::fs::write(&target, &blob).expect("plant");
        std::os::windows::fs::symlink_file(&target, &path).expect("symlink");
        let key = PanelKey::load_or_create(&path).expect("created");
        assert_ne!(key.key, planted, "the link's target was read as the key");
        assert_eq!(
            std::fs::read(&target).expect("read"),
            blob,
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

    /// CNG and DPAPI are looked up when first needed, never imported: an
    /// import puts `bcrypt.dll` and `crypt32.dll` in the executable's
    /// import table, mapped at every start whether or not anything is
    /// ever signed. A source check, because nothing else would notice —
    /// the code works either way, only slower to start.
    #[test]
    fn the_crypto_libraries_are_not_imported() {
        let src = include_str!("sys_windows.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("test module")];
        let uses = &code[code
            .find("use windows::Win32::Security::Cryptography::{")
            .expect("the types' import")..];
        let uses = &uses[..uses.find("};").expect("end of the import")];
        assert!(
            !code.contains("Cryptography::*"),
            "a glob import brings the functions in"
        );
        for name in [
            "BCryptGenRandom",
            "BCryptHash",
            "CryptProtectData",
            "CryptUnprotectData",
        ] {
            assert!(
                !uses.contains(&format!("{name},")) && !uses.contains(&format!("{name} ")),
                "{name} is imported, which maps its library at every start"
            );
            assert!(
                !code.contains(&format!("Cryptography::{name}(")),
                "{name} is called directly, which imports its library"
            );
        }
    }
}
