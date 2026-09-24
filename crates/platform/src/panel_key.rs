//! The key Code++ signs plugin panels' startup commands with — see
//! `codepp_core::dock::CommandSeal` for what is signed and why.
//!
//! A random 32-byte key, generated the first time something is signed
//! and kept at [`crate::panel_key_path`], encrypted with DPAPI for the
//! current Windows account. The file on its own is therefore useless:
//! copied to another account or machine, or handed over inside a copied
//! config directory, it does not decrypt, a fresh key replaces it, and
//! nothing the old one signed checks out.
//!
//! What the key does not stop is code already running as the user,
//! which can ask DPAPI for the key exactly as Code++ does. Such code
//! could as easily drop a DLL into the plugins directory (DESIGN.md
//! §6.5), so the line is drawn where a line can be drawn: an edited or
//! copied file cannot choose what runs; a program running as you still
//! can, as it always could.
//!
//! The signature is HMAC-SHA256 through Windows' own CNG provider, and
//! the key's randomness comes from CNG as well, so nothing here is
//! hand-rolled cryptography and no crate is added for it. Both
//! libraries are loaded the first time a key is needed rather than
//! imported — see [`Crypto`] — so a session that never signs or checks
//! anything starts exactly as it did before this module existed.
//!
//! Windows only: the other two backends host no plugin panels.

use std::cell::{Cell, OnceCell};
use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{s, w};
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    BCRYPT_HMAC_SHA256_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG, CRYPTPROTECT_UI_FORBIDDEN,
    CRYPT_INTEGER_BLOB,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

/// Bytes in the key and in a signature.
const KEY_LEN: usize = 32;

/// DPAPI's optional entropy for the key file: a second input the file
/// needs to decrypt, so a blob some other program protected for the same
/// account is not mistaken for this key, and this one is not read by a
/// program that simply asks DPAPI to decrypt whatever it finds.
const ENTROPY: &[u8] = b"Code++ plugin panel key v1";

/// Largest key file read. A DPAPI blob around a 32-byte secret is a few
/// hundred bytes; anything past this is not one of ours, and is not read
/// into memory to find that out.
const MAX_KEY_FILE_BYTES: u64 = 16 * 1024;

/// How many times [`PanelKey::load_or_create`] looks again when the
/// file changes under it. With the lock held that takes a writer that
/// does not honour it — nothing else writes the key, but the file is the
/// user's — so a second look is nearly always enough; running out is an
/// error rather than a loop.
const MAX_LOOKS: usize = 3;

/// How long [`PanelKey::load_or_create`] waits for another instance to
/// finish with the key. Another instance holds the lock for a DPAPI call
/// and a small write — milliseconds — and this wait runs on the UI
/// thread, in the middle of a plugin's registration or a load pass, so
/// it is kept short. Past it the attempt fails closed, and a
/// [`PanelSigner`] then leaves the key alone for [`RETRY_AFTER`].
const LOCK_WAIT: Duration = Duration::from_millis(500);

/// How long a [`PanelSigner`] leaves the key alone after failing to get
/// it, before trying again.
///
/// Without it every signature and every check would try afresh, and a
/// load pass makes one per restored panel — up to
/// `codepp_core::dock::MAX_RESTORED_PLUGIN_PANELS` of them. The security
/// audit showed what that costs when the key's lock is held for good:
/// any process running as the user can keep a handle open on the lock
/// file, and each attempt then waits out [`LOCK_WAIT`] on the UI thread,
/// once per panel, a minute of frozen window at startup. With it a held
/// lock costs one wait per interval; a key that could not be had for a
/// moment is still tried again later, rather than never this session.
const RETRY_AFTER: Duration = Duration::from_secs(10);

/// `CreateFileW`'s `FILE_FLAG_OPEN_REPARSE_POINT`: open a link itself
/// rather than what it points at. The same bare constant
/// `codepp_shell::fif` uses, fixed since Windows 2000.
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// `ERROR_SHARING_VIOLATION`: another handle holds the file with no
/// sharing — here, another instance holding the key's lock.
const ERROR_SHARING_VIOLATION: i32 = 32;

/// The account's key, decrypted.
pub struct PanelKey {
    key: [u8; KEY_LEN],
}

/// What is at the key's path.
enum KeyFile {
    /// Nothing: the first time a key is needed.
    Missing,
    /// A key this account can decrypt.
    Key(PanelKey),
    /// A file this account read but cannot use: it does not decrypt,
    /// decrypts to the wrong length, is too large to be a key, or is not
    /// a regular file at all — a link, which is never followed. Carries
    /// what was read, so a replacement can check nothing changed it since.
    NotOurs { blob: Vec<u8>, why: io::Error },
}

impl PanelKey {
    /// Read the key at `path`, or create one there.
    ///
    /// A missing file is created. A file this account can read but not
    /// use — copied from elsewhere, damaged, not a key at all — is
    /// replaced, with a warning, because keeping it would leave nothing
    /// able to sign. Everything the old key signed then stops checking
    /// out, which is the point: whoever wrote those signatures was not
    /// this account. A file that cannot be *read* — locked for a moment
    /// by a scanner, say — is an error and is left alone: replacing a
    /// good key over a passing I/O failure would cost every panel its
    /// signature.
    ///
    /// Two instances never do this at the same moment: each holds a lock
    /// beside the key while it reads, creates or replaces it — see
    /// [`lock_key`] — and the other waits its turn, so they end up with
    /// one key. Should a writer not honour the lock, the earlier measures
    /// still stand behind it: creating never overwrites, a replacement
    /// goes ahead only if the file still holds what was judged unusable,
    /// and the key used afterwards is the one read back from disk.
    ///
    /// # Errors
    ///
    /// The file could not be read or written, CNG or DPAPI is not
    /// available, another instance held the lock for longer than
    /// [`LOCK_WAIT`], or the file kept changing.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        Self::load_or_create_waiting(path, LOCK_WAIT)
    }

    /// [`Self::load_or_create`], waiting up to `wait` for the lock.
    fn load_or_create_waiting(path: &Path, wait: Duration) -> io::Result<Self> {
        // Without these, a file that does not decrypt says nothing about
        // the file.
        crypto()?;
        let _lock = lock_key(path, wait)?;
        for _ in 0..MAX_LOOKS {
            match read_key_file(path)? {
                KeyFile::Key(key) => return Ok(key),
                KeyFile::Missing => {
                    let key = Self::generate()?;
                    if create_key_file(path, &protect(&key.key)?)? {
                        return Ok(key);
                    }
                    // Something created it first: read that.
                }
                KeyFile::NotOurs { blob, why } => {
                    let key = Self::generate()?;
                    if replace_key_file(path, &blob, &protect(&key.key)?)? {
                        tracing::warn!(
                            path = ?path,
                            error = ?why,
                            "the plugin panel key does not decrypt for this account; replaced it"
                        );
                        // Whatever is on disk now is the key: this one, or
                        // what a writer ignoring the lock put there an
                        // instant later.
                        if let KeyFile::Key(on_disk) = read_key_file(path)? {
                            return Ok(on_disk);
                        }
                    }
                    // It changed since it was read: look again.
                }
            }
        }
        Err(io::Error::other(
            "the plugin panel key kept changing while it was being created",
        ))
    }

    fn generate() -> io::Result<Self> {
        Ok(Self { key: random_key()? })
    }

    /// HMAC-SHA256 of `message` under this key. `None` only if CNG
    /// fails, which the caller treats as "cannot sign".
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Option<[u8; KEY_LEN]> {
        hmac_sha256(&self.key, message)
    }
}

impl Drop for PanelKey {
    fn drop(&mut self) {
        // Hygiene rather than defence: a plugin runs in this process and
        // can read its memory. Volatile so the stores are not elided.
        for byte in &mut self.key {
            // SAFETY: `byte` is a valid, aligned, exclusive reference.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

/// A [`PanelKey`] loaded the first time it is needed, and kept.
///
/// Nothing is signed or checked at all in a session that has no plugin
/// panel, so a session like that never touches the file, DPAPI or the
/// libraries they live in.
///
/// A failure is not kept for good, but for [`RETRY_AFTER`]: calls made
/// meanwhile answer `None` at once, and the next one after it tries
/// again. So a key that could not be had for a moment costs only the
/// calls made meanwhile — with Preferences → Security's guard on, those
/// panels are not restored by command, and one registered meanwhile is
/// recorded unsigned — and a key that cannot be had at all costs one
/// attempt per interval rather than one per call. The first failure is
/// a warning and the rest are debug lines, so a key that can never be
/// made does not fill the log.
pub struct PanelSigner {
    path: Option<PathBuf>,
    key: OnceCell<PanelKey>,
    /// When the last attempt failed, while [`Self::retry_after`] has not
    /// yet passed since.
    failed_at: Cell<Option<Instant>>,
    lock_wait: Duration,
    retry_after: Duration,
    warned: Cell<bool>,
}

impl PanelSigner {
    /// A signer for the key at `path`. `None` signs nothing — the case
    /// where there is no config directory to keep a key in.
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self::with_timing(path, LOCK_WAIT, RETRY_AFTER)
    }

    fn with_timing(path: Option<PathBuf>, lock_wait: Duration, retry_after: Duration) -> Self {
        Self {
            path,
            key: OnceCell::new(),
            failed_at: Cell::new(None),
            lock_wait,
            retry_after,
            warned: Cell::new(false),
        }
    }

    /// HMAC-SHA256 of `message`, or `None` when there is no key.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Option<[u8; KEY_LEN]> {
        if let Some(key) = self.key.get() {
            return key.sign(message);
        }
        let path = self.path.as_deref()?;
        if self
            .failed_at
            .get()
            .is_some_and(|at| at.elapsed() < self.retry_after)
        {
            return None;
        }
        match PanelKey::load_or_create_waiting(path, self.lock_wait) {
            Ok(key) => {
                self.failed_at.set(None);
                self.key.get_or_init(|| key).sign(message)
            }
            Err(e) => {
                self.failed_at.set(Some(Instant::now()));
                if self.warned.replace(true) {
                    tracing::debug!(path = ?path, error = ?e, "still no plugin panel key");
                } else {
                    tracing::warn!(
                        path = ?path,
                        error = ?e,
                        "no plugin panel key; startup commands cannot be signed or checked"
                    );
                }
                None
            }
        }
    }
}

/// What is at `path` — see [`KeyFile`]. An error only when the file
/// could not be read at all.
fn read_key_file(path: &Path) -> io::Result<KeyFile> {
    let Some(blob) = read_capped(path)? else {
        return Ok(KeyFile::Missing);
    };
    if blob.is_empty() {
        return Ok(KeyFile::NotOurs {
            blob,
            why: io::Error::new(io::ErrorKind::InvalidData, "empty, or not a regular file"),
        });
    }
    if blob.len() as u64 > MAX_KEY_FILE_BYTES {
        return Ok(KeyFile::NotOurs {
            blob,
            why: io::Error::new(io::ErrorKind::InvalidData, "larger than any key file"),
        });
    }
    let mut key = [0u8; KEY_LEN];
    match unprotect_into(&blob, &mut key) {
        Ok(()) => Ok(KeyFile::Key(PanelKey { key })),
        Err(why) => Ok(KeyFile::NotOurs { blob, why }),
    }
}

/// The bytes at `path`, at most [`MAX_KEY_FILE_BYTES`] + 1 of them, so
/// an oversized file is recognised without being read whole; `None`
/// when there is no file.
///
/// A link planted at the path is not followed: the handle is to the
/// link itself, which counts as holding no bytes, so what it points at
/// is never read and the link is replaced like any other file that is
/// not a key. A link *above* the key — a whole config directory moved
/// with a junction — is followed as usual; only the last component is
/// opened this way.
fn read_capped(path: &Path) -> io::Result<Option<Vec<u8>>> {
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
        return Ok(Some(Vec::new()));
    }
    let mut blob = Vec::new();
    file.take(MAX_KEY_FILE_BYTES + 1).read_to_end(&mut blob)?;
    Ok(Some(blob))
}

/// `blob` written to a temporary file beside `path` and synced, ready
/// to be renamed over it — so the key file is never seen half-written.
fn staged(path: &Path, blob: &[u8]) -> io::Result<tempfile::NamedTempFile> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".panel-key-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    tmp.write_all(blob)?;
    tmp.as_file().sync_all()?;
    Ok(tmp)
}

/// Create the key file at `path` holding `blob`. An existing file is
/// left alone and `Ok(false)` says so. There is deliberately no way to
/// overwrite unconditionally: replacing a key goes through
/// [`replace_key_file`], which checks what it replaces.
fn create_key_file(path: &Path, blob: &[u8]) -> io::Result<bool> {
    let tmp = staged(path, blob)?;
    match tmp.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.error),
    }
}

/// Replace the key file at `path` with `blob`, unless it no longer
/// holds `seen` — another instance having replaced it since — in which
/// case nothing is written and `Ok(false)` says so. The new file is
/// staged first so the check and the rename sit as close together as
/// they can; they are still two steps, which is why
/// [`PanelKey::load_or_create`] reads back what won.
fn replace_key_file(path: &Path, seen: &[u8], blob: &[u8]) -> io::Result<bool> {
    let tmp = staged(path, blob)?;
    if read_capped(path)?.as_deref() != Some(seen) {
        return Ok(false);
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(true)
}

/// Take the lock that keeps two instances from reading, creating or
/// replacing the key at the same moment, waiting up to `wait` for it.
///
/// It is a file beside the key, `panel-restore.key.lock`, opened with no
/// sharing: a second instance's open fails with a sharing violation for
/// as long as the first holds it. Holding the handle is the lock, and
/// the system closes a dying process's handles, so nothing is ever left
/// locked. Like the key, a link planted at the lock's path is opened
/// itself rather than followed, and nothing is ever written to it.
fn lock_key(path: &Path, wait: Duration) -> io::Result<std::fs::File> {
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the key has no file name"))?
        .to_os_string();
    name.push(".lock");
    let lock_path = path.with_file_name(name);
    if let Some(dir) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let deadline = Instant::now() + wait;
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&lock_path)
        {
            Ok(file) => return Ok(file),
            Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "something held the plugin panel key's lock for longer than {wait:?}"
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
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
fn random_key() -> io::Result<[u8; KEY_LEN]> {
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
fn hmac_sha256(key: &[u8], message: &[u8]) -> Option<[u8; KEY_LEN]> {
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
mod tests {
    use super::*;

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// Hold `path` open with no sharing, so every other open fails — a
    /// scanner's momentary lock, reproducibly.
    fn lock(path: &Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .expect("lock")
    }

    /// The MAC is HMAC-SHA256 and nothing else: RFC 4231's test cases
    /// 1, 2 and 6 — a short key, a key shorter than the output, and a
    /// key longer than SHA-256's block, which HMAC hashes first.
    #[test]
    fn the_mac_is_hmac_sha256() {
        let cases: [(Vec<u8>, &[u8], &str); 3] = [
            (
                vec![0x0b; 20],
                b"Hi There",
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
        ];
        for (key, message, expected) in cases {
            assert_eq!(
                hmac_sha256(&key, message).expect("CNG signs").to_vec(),
                unhex(expected)
            );
        }
    }

    /// A key is created once and read back the same, and what is on
    /// disk is not the key.
    #[test]
    fn a_created_key_reads_back_and_is_not_stored_in_the_clear() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub").join("panel-restore.key");
        let first = PanelKey::load_or_create(&path).expect("create");
        let blob = std::fs::read(&path).expect("written");
        assert!(blob.len() > KEY_LEN, "a DPAPI blob, not the bare key");
        assert!(
            !blob.windows(KEY_LEN).any(|w| w == first.key),
            "the key is not in the file in the clear"
        );
        let again = PanelKey::load_or_create(&path).expect("read");
        assert_eq!(first.key, again.key);
        assert_eq!(first.sign(b"panel"), again.sign(b"panel"));
        assert_ne!(first.sign(b"panel"), first.sign(b"other"));
    }

    /// A file this account cannot use is replaced rather than kept:
    /// garbage, a real DPAPI blob made for another purpose — one
    /// protected without this key's entropy — and one too large to be a
    /// key at all.
    #[test]
    fn a_key_file_this_account_cannot_use_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let secret = [7u8; KEY_LEN];
        let foreign = protect_with(&secret, None).expect("protect");
        let oversized = vec![0u8; usize::try_from(MAX_KEY_FILE_BYTES).expect("fits") + 1];
        for planted in [vec![0x42; 100], foreign, oversized] {
            std::fs::write(&path, &planted).expect("plant");
            let key = PanelKey::load_or_create(&path).expect("replaced");
            assert_ne!(key.key, secret);
            assert_ne!(std::fs::read(&path).expect("read"), planted, "replaced");
            assert_eq!(
                PanelKey::load_or_create(&path).expect("read").key,
                key.key,
                "and the replacement is what is read next time"
            );
        }
    }

    /// A file that cannot be read is not judged at all: it is left as it
    /// is, and read once it can be. A passing lock must not cost the
    /// key.
    #[test]
    fn a_key_file_that_cannot_be_read_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let original = PanelKey::load_or_create(&path).expect("create");
        let written = std::fs::read(&path).expect("read");
        let held = lock(&path);
        assert!(
            read_key_file(&path).is_err(),
            "a file that could not be read is an error, not a file judged unusable"
        );
        assert!(PanelKey::load_or_create(&path).is_err());
        drop(held);
        assert_eq!(std::fs::read(&path).expect("read"), written, "untouched");
        assert_eq!(
            PanelKey::load_or_create(&path).expect("read").key,
            original.key
        );
    }

    /// Losing the race to create the key means using the winner's.
    #[test]
    fn a_key_created_meanwhile_is_not_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let winner = PanelKey::load_or_create(&path).expect("create");
        let written = std::fs::read(&path).expect("read");
        assert!(!create_key_file(&path, b"loser").expect("write"));
        assert_eq!(std::fs::read(&path).expect("read"), written);
        assert_eq!(
            PanelKey::load_or_create(&path).expect("read").key,
            winner.key
        );
    }

    /// Two instances take turns with the key: one holding its lock makes
    /// the other wait, and give up once the wait runs out; when it lets
    /// go, the other goes ahead and reads the same key.
    #[test]
    fn instances_take_turns_with_the_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let key = PanelKey::load_or_create(&path).expect("create");
        let held = lock_key(&path, LOCK_WAIT).expect("lock");
        assert!(
            PanelKey::load_or_create_waiting(&path, Duration::from_millis(50)).is_err(),
            "a held lock is waited for, then given up on"
        );
        let waiting = {
            let path = path.clone();
            std::thread::spawn(move || {
                PanelKey::load_or_create_waiting(&path, Duration::from_secs(10)).map(|k| k.key)
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        drop(held);
        assert_eq!(
            waiting.join().expect("join").expect("goes ahead once free"),
            key.key
        );
    }

    /// A hard link planted at the key's path is replaced, not written
    /// through: the file it shares its data with keeps what it held.
    /// Making one needs no privilege, so this is the planted-link case
    /// that runs everywhere.
    #[test]
    fn a_hard_link_at_the_keys_path_is_replaced_not_written_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let target = dir.path().join("precious.txt");
        std::fs::write(&target, b"precious").expect("plant");
        std::fs::hard_link(&target, &path).expect("link");
        let key = PanelKey::load_or_create(&path).expect("replaced");
        assert_eq!(std::fs::read(&target).expect("read"), b"precious");
        assert_eq!(PanelKey::load_or_create(&path).expect("read").key, key.key);
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

    /// A replacement is made only over what was judged unusable: a file
    /// another instance has replaced since is left for that instance.
    #[test]
    fn a_replacement_is_skipped_when_the_file_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        std::fs::write(&path, b"judged unusable").expect("plant");
        assert!(!replace_key_file(&path, b"something else", b"new").expect("replace"));
        assert_eq!(std::fs::read(&path).expect("read"), b"judged unusable");
        assert!(replace_key_file(&path, b"judged unusable", b"new").expect("replace"));
        assert_eq!(std::fs::read(&path).expect("read"), b"new");
    }

    /// A signer loads its key once, on first use, and keeps it; with no
    /// path it signs nothing.
    #[test]
    fn a_signer_loads_its_key_once() {
        assert_eq!(PanelSigner::new(None).sign(b"panel"), None);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let signer = PanelSigner::new(Some(path.clone()));
        assert!(!path.exists(), "nothing is created before it is needed");
        let first = signer.sign(b"panel").expect("signs");
        assert!(path.exists());
        std::fs::remove_file(&path).expect("remove");
        assert_eq!(signer.sign(b"panel"), Some(first), "the key is kept");
        assert_ne!(
            PanelSigner::new(Some(path)).sign(b"panel"),
            Some(first),
            "a new key signs differently"
        );
    }

    /// CNG and DPAPI are looked up when first needed, never imported: an
    /// import puts `bcrypt.dll` and `crypt32.dll` in the executable's
    /// import table, mapped at every start whether or not anything is
    /// ever signed. A source check, because nothing else would notice —
    /// the code works either way, only slower to start.
    #[test]
    fn the_crypto_libraries_are_not_imported() {
        let src = include_str!("panel_key.rs");
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

    /// A failure is not kept for good: a signer whose key could not be
    /// read leaves it alone for the interval, then signs again — with
    /// the same key.
    #[test]
    fn a_signer_tries_again_after_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let key = PanelKey::load_or_create(&path).expect("create");
        // Margins wide enough for a starved test runner: nothing here
        // takes more than a few milliseconds when it goes right.
        let signer =
            PanelSigner::with_timing(Some(path.clone()), LOCK_WAIT, Duration::from_secs(1));
        let held = lock(&path);
        assert_eq!(signer.sign(b"panel"), None);
        drop(held);
        assert_eq!(
            signer.sign(b"panel"),
            None,
            "left alone until the interval has passed"
        );
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(signer.sign(b"panel"), key.sign(b"panel"));
    }

    /// A lock something holds for good — any handle open on the lock
    /// file, from any process running as the user — costs one wait per
    /// interval, not one per call: a load pass checking dozens of panels
    /// must not wait out the lock for each of them on the UI thread.
    #[test]
    fn a_held_lock_costs_one_wait_per_interval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let key = PanelKey::load_or_create(&path).expect("create");
        let holder = std::fs::File::open(dir.path().join("panel-restore.key.lock")).expect("hold");
        // Thirty calls that each waited would take three seconds; the
        // bound below is a third of that, and the interval outlasts it,
        // so a starved runner has a second to spare before either trips.
        let signer = PanelSigner::with_timing(
            Some(path.clone()),
            Duration::from_millis(100),
            Duration::from_millis(1500),
        );
        let started = Instant::now();
        assert_eq!(signer.sign(b"panel"), None);
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the first call waits"
        );
        let started = Instant::now();
        for _ in 0..30 {
            assert_eq!(signer.sign(b"panel"), None);
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the rest do not wait: {:?}",
            started.elapsed()
        );
        drop(holder);
        std::thread::sleep(Duration::from_millis(1700));
        assert_eq!(signer.sign(b"panel"), key.sign(b"panel"));
    }

    /// A hard link planted at the lock's path is opened but never
    /// written through: the file it shares its data with keeps what it
    /// held. The lock asks for write access, which is why this is pinned.
    #[test]
    fn a_hard_link_at_the_locks_path_is_not_written_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let target = dir.path().join("precious.txt");
        std::fs::write(&target, b"precious").expect("plant");
        std::fs::hard_link(&target, dir.path().join("panel-restore.key.lock")).expect("link");
        let key = PanelKey::load_or_create(&path).expect("create");
        assert_eq!(std::fs::read(&target).expect("read"), b"precious");
        assert_eq!(PanelKey::load_or_create(&path).expect("read").key, key.key);
    }
}
