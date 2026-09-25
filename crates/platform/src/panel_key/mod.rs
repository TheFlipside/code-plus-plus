//! The key Code++ signs plugin panels' startup commands with — see
//! `codepp_core::dock::CommandSeal` for what is signed and why.
//!
//! A random 32-byte key, generated the first time something is signed
//! and kept at [`crate::panel_key_path`]. How the file protects it is each
//! platform's own business, in the backend beside this module:
//!
//!   * **Windows** (`sys_windows`) encrypts it with DPAPI for the current
//!     account. The file on its own is useless: copied to another
//!     account or machine, or handed over inside a copied config
//!     directory, it does not decrypt, a fresh key replaces it, and
//!     nothing the old one signed checks out. The randomness and the
//!     HMAC-SHA256 come from CNG.
//!   * **Linux and macOS** (`sys_unix`) have no DPAPI, so the file holds
//!     the key itself and the protection is the file's: it must be a
//!     regular file owned by the user's account, carrying no permissions
//!     for any other — on macOS none granted by an extended ACL either,
//!     since those do not show in the mode bits. One that is not — copied
//!     in with lax permissions, left by another account — is not trusted,
//!     and is replaced. The randomness comes from the kernel (`getrandom`,
//!     `getentropy`) and the HMAC-SHA256 from a library the backend has
//!     loaded anyway (`GLib` under GTK, `CommonCrypto` in `libSystem`).
//!     What this cannot match is DPAPI's refusal to travel: a whole
//!     config directory copied *with* the key file carries the key along,
//!     so the session file it signed checks out on the other side. A
//!     session file edited, or handed over, on its own still cannot
//!     choose what runs.
//!
//! What the key does not stop, on any platform, is code already running
//! as the user, which can get the key exactly as Code++ does. Such code
//! could as easily drop a plugin into the plugins directory (DESIGN.md
//! §6.5), so the line is drawn where a line can be drawn: an edited or
//! copied file cannot choose what runs; a program running as you still
//! can, as it always could.
//!
//! Nothing here is hand-rolled cryptography, and no crate is added for
//! it: each platform's own primitives do the work.

use std::cell::{Cell, OnceCell};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod sys_unix;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use sys_unix as sys;
#[cfg(target_os = "windows")]
mod sys_windows;
#[cfg(target_os = "windows")]
use sys_windows as sys;

/// Bytes in the key and in a signature.
const KEY_LEN: usize = 32;

/// Largest key file read. A DPAPI blob around a 32-byte secret is a few
/// hundred bytes, and a Unix key file is the 32 bytes themselves;
/// anything past this is not one of ours, and is not read into memory to
/// find that out.
const MAX_KEY_FILE_BYTES: u64 = 16 * 1024;

/// How many times [`PanelKey::load_or_create`] looks again when the
/// file changes under it. With the lock held that takes a writer that
/// does not honour it — nothing else writes the key, but the file is the
/// user's — so a second look is nearly always enough; running out is an
/// error rather than a loop.
const MAX_LOOKS: usize = 3;

/// How long [`PanelKey::load_or_create`] waits for another instance to
/// finish with the key. Another instance holds the lock for a small read
/// or write — milliseconds — and this wait runs on the UI thread, in the
/// middle of a plugin's registration or a load pass, so it is kept
/// short. Past it the attempt fails closed, and a [`PanelSigner`] then
/// leaves the key alone for [`RETRY_AFTER`].
const LOCK_WAIT: Duration = Duration::from_millis(500);

/// How long a [`PanelSigner`] leaves the key alone after failing to get
/// it, before trying again.
///
/// Without it every signature and every check would try afresh, and a
/// load pass makes one per restored panel — up to
/// `codepp_core::dock::MAX_RESTORED_PLUGIN_PANELS` of them. The security
/// audit showed what that costs when the key's lock is held for good:
/// any process running as the user can keep the lock file open (Windows)
/// or locked (Linux, macOS), and each attempt then waits out [`LOCK_WAIT`] on
/// the UI thread, once per panel, a minute of frozen window at startup.
/// With it a held lock costs one wait per interval; a key that could not
/// be had for a moment is still tried again later, rather than never this
/// session.
const RETRY_AFTER: Duration = Duration::from_secs(10);

/// The account's key, in the clear.
pub struct PanelKey {
    key: [u8; KEY_LEN],
}

/// What a backend's `read_capped` found at the key's path.
struct Blob {
    /// The file's bytes, at most [`MAX_KEY_FILE_BYTES`] + 1 of them —
    /// empty for something that is not a regular file, which is never
    /// read.
    bytes: Vec<u8>,
    /// Why the file cannot be the key whatever it holds — it is not a
    /// regular file, or (Unix) another account owns it or could read it
    /// — or `None` when only its contents decide.
    unusable: Option<io::Error>,
}

impl Blob {
    /// A path holding something that is not a key file at all.
    fn unusable(why: &str) -> Self {
        Self {
            bytes: Vec::new(),
            unusable: Some(io::Error::new(io::ErrorKind::InvalidData, why)),
        }
    }
}

/// What is at the key's path.
enum KeyFile {
    /// Nothing: the first time a key is needed.
    Missing,
    /// A key this account can use.
    Key(PanelKey),
    /// A file this account read but cannot use: its contents are not a
    /// key for this account, it is too large to be a key, it is not a
    /// regular file at all — a link, which is never followed — or
    /// (Unix) its ownership, its permissions or (macOS) its ACL let
    /// another account read or change it. Carries what was read, so a replacement can check
    /// nothing changed it since — as a [`Sealed`], wiped when dropped,
    /// because a file this account cannot use may still hold a real key.
    NotOurs { blob: Sealed, why: io::Error },
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
    /// The file could not be read or written, the platform's primitives
    /// are not available, another instance held the lock for longer than
    /// [`LOCK_WAIT`], or the file kept changing.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        Self::load_or_create_waiting(path, LOCK_WAIT)
    }

    /// [`Self::load_or_create`], waiting up to `wait` for the lock.
    fn load_or_create_waiting(path: &Path, wait: Duration) -> io::Result<Self> {
        // Without these, a file that does not open says nothing about the
        // file.
        sys::ready()?;
        let _lock = lock_key(path, wait)?;
        for _ in 0..MAX_LOOKS {
            match read_key_file(path)? {
                KeyFile::Key(key) => return Ok(key),
                KeyFile::Missing => {
                    let key = Self::generate()?;
                    if create_key_file(path, &Sealed::of(&key)?)? {
                        return Ok(key);
                    }
                    // Something created it first: read that.
                }
                KeyFile::NotOurs { blob, why } => {
                    let key = Self::generate()?;
                    if replace_key_file(path, &blob, &Sealed::of(&key)?)? {
                        tracing::warn!(
                            path = ?path,
                            error = ?why,
                            "the plugin panel key is not usable by this account; replaced it"
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
        Ok(Self {
            key: sys::random_key()?,
        })
    }

    /// HMAC-SHA256 of `message` under this key. `None` only if the
    /// platform's MAC fails, which the caller treats as "cannot sign".
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Option<[u8; KEY_LEN]> {
        sys::hmac_sha256(&self.key, message)
    }
}

impl Drop for PanelKey {
    fn drop(&mut self) {
        // Hygiene rather than defence: a plugin runs in this process and
        // can read its memory.
        wipe(&mut self.key);
    }
}

/// A key in the form its file holds it — a DPAPI blob on Windows, the
/// key itself on Linux and macOS — wiped when dropped, since there it
/// *is* the key. Also what a key file held that this account could not
/// use ([`KeyFile::NotOurs`]): its permissions or its owner can be wrong
/// while its bytes are still a key.
struct Sealed(Vec<u8>);

impl Sealed {
    fn of(key: &PanelKey) -> io::Result<Self> {
        sys::seal(&key.key).map(Self)
    }
}

impl std::ops::Deref for Sealed {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Sealed {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

/// Overwrite `bytes` with zeros, in a way the compiler may not elide.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: `byte` is a valid, aligned, exclusive reference.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
}

/// A [`PanelKey`] loaded the first time it is needed, and kept.
///
/// Nothing is signed or checked at all in a session that has no plugin
/// panel, so a session like that never touches the file or the
/// libraries the platform signs with.
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
    let Some(Blob {
        mut bytes,
        unusable,
    }) = sys::read_capped(path)?
    else {
        return Ok(KeyFile::Missing);
    };
    if let Some(why) = unusable {
        return Ok(KeyFile::NotOurs {
            blob: Sealed(bytes),
            why,
        });
    }
    if bytes.is_empty() {
        return Ok(KeyFile::NotOurs {
            blob: Sealed(bytes),
            why: io::Error::new(io::ErrorKind::InvalidData, "empty"),
        });
    }
    if bytes.len() as u64 > MAX_KEY_FILE_BYTES {
        return Ok(KeyFile::NotOurs {
            blob: Sealed(bytes),
            why: io::Error::new(io::ErrorKind::InvalidData, "larger than any key file"),
        });
    }
    let mut key = [0u8; KEY_LEN];
    match sys::unseal(&bytes, &mut key) {
        Ok(()) => {
            // On Linux and macOS the file's bytes are the key itself.
            wipe(&mut bytes);
            Ok(KeyFile::Key(PanelKey { key }))
        }
        Err(why) => Ok(KeyFile::NotOurs {
            blob: Sealed(bytes),
            why,
        }),
    }
}

/// `blob` written to a temporary file beside `path` and synced, ready
/// to be renamed over it — so the key file is never seen half-written.
/// On Unix the temporary file is created readable and writable by its
/// owner alone, so the key is never on disk with looser permissions.
/// That is `tempfile`'s default rather than something set here; the
/// Unix tests check the mode the key file ends up with, so a change in
/// that default fails them instead of loosening the key. What a mode
/// cannot express is the platform's to remove — see `make_private` —
/// before a byte of the key is written.
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
    sys::make_private(tmp.as_file())?;
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
///
/// The rename replaces whatever is at `path` — a link included, which
/// it replaces rather than writing through.
fn replace_key_file(path: &Path, seen: &[u8], blob: &[u8]) -> io::Result<bool> {
    let tmp = staged(path, blob)?;
    // A [`Sealed`], for the reason [`KeyFile::NotOurs`] carries one: what
    // is there now may be a key — another instance's replacement, say.
    let now = sys::read_capped(path)?.map(|b| Sealed(b.bytes));
    if now.as_deref() != Some(seen) {
        return Ok(false);
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(true)
}

/// Take the lock that keeps two instances from reading, creating or
/// replacing the key at the same moment, waiting up to `wait` for it.
///
/// It is a file beside the key, `panel-restore.key.lock`, and holding
/// it is each platform's own — see the backend's `try_lock`. Either way
/// the system releases a dying process's hold, so nothing is ever left
/// locked, and nothing is ever written to the file.
fn lock_key(path: &Path, wait: Duration) -> io::Result<std::fs::File> {
    let lock_path = lock_path_for(path)?;
    if let Some(dir) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let deadline = Instant::now() + wait;
    loop {
        if let Some(held) = sys::try_lock(&lock_path)? {
            return Ok(held);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("something held the plugin panel key's lock for longer than {wait:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `panel-restore.key.lock` beside the key at `path`.
fn lock_path_for(path: &Path) -> io::Result<PathBuf> {
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the key has no file name"))?
        .to_os_string();
    name.push(".lock");
    Ok(path.with_file_name(name))
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
                sys::hmac_sha256(&key, message)
                    .expect("the platform signs")
                    .to_vec(),
                unhex(expected)
            );
        }
    }

    /// A key is created once and read back the same, and signs
    /// consistently.
    #[test]
    fn a_created_key_reads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub").join("panel-restore.key");
        let first = PanelKey::load_or_create(&path).expect("create");
        let again = PanelKey::load_or_create(&path).expect("read");
        assert_eq!(first.key, again.key);
        assert_eq!(first.sign(b"panel"), again.sign(b"panel"));
        assert_ne!(first.sign(b"panel"), first.sign(b"other"));
    }

    /// A file that cannot be a key is replaced rather than kept: garbage,
    /// and one too large to be a key at all. Each backend adds the cases
    /// only it can make — a DPAPI blob for another purpose, a file other
    /// accounts could read.
    #[test]
    fn a_key_file_that_is_not_a_key_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let oversized = vec![0u8; usize::try_from(MAX_KEY_FILE_BYTES).expect("fits") + 1];
        for planted in [vec![0x42; 100], oversized] {
            std::fs::write(&path, &planted).expect("plant");
            let key = PanelKey::load_or_create(&path).expect("replaced");
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
        let Some(held) = sys::test_support::make_unreadable(&path) else {
            eprintln!("skipping: this account can read any file (root?)");
            return;
        };
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
        let Some(held) = sys::test_support::make_unreadable(&path) else {
            eprintln!("skipping: this account can read any file (root?)");
            return;
        };
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

    /// A lock something holds for good — from any process running as
    /// the user — costs one wait per interval, not one per call: a load
    /// pass checking dozens of panels must not wait out the lock for each
    /// of them on the UI thread.
    #[test]
    fn a_held_lock_costs_one_wait_per_interval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panel-restore.key");
        let key = PanelKey::load_or_create(&path).expect("create");
        let holder = sys::test_support::hold_lock(&lock_path_for(&path).expect("lock path"));
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
