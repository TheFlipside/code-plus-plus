//! Verifying a downloaded update and handing it to Windows Installer
//! (DESIGN.md §11.4).
//!
//! `WinSparkle` downloads an installer, verifies its signature, and then
//! hands Code++ its path, leaving the file in the user's temporary
//! folder, where any of the user's programs can rewrite it (measured,
//! DESIGN.md §11.11). So the path is not taken on trust. [`verify`]
//! opens the file so that nothing else can write, rename or delete it,
//! verifies the signature itself on the bytes it reads through that
//! handle, and only then asks the package for its version.
//! [`VerifiedInstaller::launch`] starts `msiexec.exe` with that handle
//! inherited, so the file stays as it was verified until the installer's
//! process ends, however soon Code++ itself exits.
//!
//! **What the open handle holds still** (each measured, DESIGN.md
//! §11.11). The file is opened sharing reads only. While it is open so,
//! no other open for writing or deleting succeeds, the file cannot be
//! renamed or replaced, and no folder above it can be renamed, so the
//! path keeps naming this file. The open itself fails while anything
//! else has the file open for writing, or has a writable mapping of it,
//! so a program cannot keep a way in from before. Windows Installer can
//! still read the file through its path.
//!
//! **How far that is shown.** For an installation for one account, a
//! real `msiexec.exe` has installed a package held this way, with the
//! file refused to a writer until its process ended (DESIGN.md §11.11).
//! For an installation for the whole machine the installing is done by
//! the Windows Installer service, elevated, after a consent prompt, and
//! nothing here has been run that way: whether the service reads the
//! held file and no other has to be shown first (DESIGN.md §11.10).
//! Until it is, [`PER_MACHINE_UPDATES`] is false and
//! [`VerifiedInstaller::launch`] refuses such an installation.
//!
//! Nothing here is on the way to the first frame, and everything here
//! blocks: it is for the updater's worker thread (DESIGN.md §11.2).

use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};
use windows::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, GetFileType,
    GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FILE_TYPE_DISK,
    GETFINALPATHNAMEBYHANDLE_FLAGS,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

use crate::msi_package::product_version;
use crate::update_signature::{verifies, PUBLIC_KEY_LEN, SIGNATURE_LEN};

/// The largest installer [`verify`] reads. The whole file is held in
/// memory to be verified, and Code++'s own installer is a few
/// megabytes.
pub const MAX_INSTALLER_BYTES: u64 = 256 * 1024 * 1024;

/// Whether an installation for the whole machine may update itself.
///
/// False until it has been shown, with a real elevated installation,
/// that what Windows Installer's service installs is the file
/// [`verify`] holds and no other (DESIGN.md §11.4, §11.10). An
/// installation for one account runs nothing elevated, so a program of
/// that account gains nothing by getting round the hold.
pub const PER_MACHINE_UPDATES: bool = false;

/// The registry key and value in which the `.msi` records the folder it
/// installed into, under `HKEY_LOCAL_MACHINE` for an installation for
/// the whole machine and `HKEY_CURRENT_USER` for one account's.
const INSTALLER_KEY: windows::core::PCWSTR = w!("Software\\Code++\\Installer");
const INSTALL_FOLDER_VALUE: windows::core::PCWSTR = w!("InstallFolder");

/// `FILE_SHARE_READ`: others may read the file, and nothing else.
const SHARE_READ_ONLY: u32 = 0x1;
/// `FILE_FLAG_OPEN_REPARSE_POINT`: open a link itself, not what it
/// points at, so that a link is seen for what it is and refused.
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// How many times the download is opened before a sharing violation is
/// taken for an answer, and how long apart. An antivirus scanner or
/// the indexer may have a file that was just written open for a
/// moment, and that moment must not cost the user the update. A
/// program that keeps the file open for writing, or keeps a writable
/// mapping of it, is still refused, two seconds later.
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY_WAIT: Duration = Duration::from_millis(200);
/// `ERROR_SHARING_VIOLATION`.
const SHARING_VIOLATION: i32 = 32;

/// The longest path handed to `msiexec.exe`, in UTF-16 units: short
/// enough for Windows to take as written. The file `WinSparkle`
/// downloads is in the user's temporary folder, far below this.
const MAX_PLAIN_PATH_UNITS: usize = 240;

/// Which kind of installation a copy of Code++ is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// Installed for the whole machine, into a folder only an
    /// administrator can write. Updating it needs elevation.
    PerMachine,
    /// Installed for one account, with that account's rights.
    PerUser,
}

/// Which kind of installation the Code++ in `exe_dir` is, or `None` if
/// Windows Installer did not put it there: a copy unpacked from the
/// zip, a build tree.
///
/// The `.msi` records the folder it installed into in the registry,
/// under the root its kind calls for. The root whose record names
/// `exe_dir` is the kind. `exe_dir` must be a folder under a drive
/// letter (`C:\…`): a copy run from a share by its network path is not
/// an installation, whatever a record says. A share mapped to a letter
/// is not told apart from a disk. `HKEY_LOCAL_MACHINE` is asked first: an
/// unprivileged program cannot write there, so a record planted under
/// `HKEY_CURRENT_USER` cannot make an installation for the whole
/// machine pass for one account's. The record is only compared, and
/// never becomes part of a command line.
#[must_use]
pub fn install_kind(exe_dir: &Path) -> Option<InstallKind> {
    kind_from(exe_dir, recorded_folder)
}

/// [`install_kind`], with the registry behind `recorded`.
fn kind_from(
    exe_dir: &Path,
    recorded: impl Fn(InstallKind) -> Option<String>,
) -> Option<InstallKind> {
    let exe_dir = exe_dir.to_str()?;
    let on_a_drive = matches!(
        exe_dir.as_bytes(),
        [letter, b':', b'\\', ..] if letter.is_ascii_alphabetic()
    );
    if !on_a_drive {
        return None;
    }
    [InstallKind::PerMachine, InstallKind::PerUser]
        .into_iter()
        .find(|kind| recorded(*kind).is_some_and(|folder| same_folder(&folder, exe_dir)))
}

/// Whether two folder paths are the same text, but for letter case and
/// trailing separators. The installer records its folder with a
/// trailing `\`, and Windows compares names regardless of case. Nothing
/// else is resolved: a path that names the same folder by another
/// spelling (a short name, a link) is not the same, which leaves such a
/// copy without an updater and nothing worse.
fn same_folder(a: &str, b: &str) -> bool {
    fn plain(path: &str) -> String {
        path.trim_end_matches(['\\', '/']).to_lowercase()
    }
    let (a, b) = (plain(a), plain(b));
    !a.is_empty() && a == b
}

/// The folder the `.msi` recorded under `kind`'s registry root.
fn recorded_folder(kind: InstallKind) -> Option<String> {
    /// The buffer's size in UTF-16 units: four times the longest path
    /// Windows takes as written.
    const UNITS: u32 = 1024;
    let root: HKEY = match kind {
        InstallKind::PerMachine => HKEY_LOCAL_MACHINE,
        InstallKind::PerUser => HKEY_CURRENT_USER,
    };
    let mut units = [0u16; UNITS as usize];
    let mut bytes = UNITS * 2;
    // SAFETY: the key and value names are NUL-terminated constants, and
    // `units` is passed with its size in bytes. `RRF_RT_REG_SZ` makes
    // the call fail for a value that is not a string, and terminate the
    // string it returns.
    let status = unsafe {
        RegGetValueW(
            root,
            INSTALLER_KEY,
            INSTALL_FOLDER_VALUE,
            RRF_RT_REG_SZ,
            None,
            Some(units.as_mut_ptr().cast()),
            Some(&raw mut bytes),
        )
    };
    if status.is_err() {
        return None;
    }
    let len = units.iter().position(|unit| *unit == 0)?;
    String::from_utf16(&units[..len]).ok()
}

/// Why an update was not verified or not started.
#[derive(Debug)]
pub enum InstallError {
    /// The file could not be opened so that nothing else can write it.
    /// Another program having it open for writing ends here.
    Open(std::io::Error),
    /// The path names something other than a plain file: a link, a
    /// folder, a device.
    NotAFile,
    /// The file is larger than [`MAX_INSTALLER_BYTES`].
    TooLarge,
    /// The file could not be read.
    Read(std::io::Error),
    /// None of the feed's signatures is the public key's signature of
    /// this file.
    BadSignature,
    /// The file's own path could not be had in a form to hand to
    /// Windows Installer: what is wrong with it.
    Path(&'static str),
    /// The file is not a package that states a version.
    NoVersion,
    /// The package's version was not accepted: the version.
    VersionRefused(String),
    /// The installation is for the whole machine, which
    /// [`PER_MACHINE_UPDATES`] does not allow yet.
    PerMachine,
    /// `msiexec.exe` could not be started.
    Launch(std::io::Error),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(error) => write!(f, "the downloaded update could not be opened: {error}"),
            Self::NotAFile => f.write_str("the downloaded update is not a plain file"),
            Self::TooLarge => f.write_str("the downloaded update is larger than an update may be"),
            Self::Read(error) => write!(f, "the downloaded update could not be read: {error}"),
            Self::BadSignature => {
                f.write_str("the downloaded update is not signed by the Code++ project")
            }
            Self::Path(why) => write!(f, "the downloaded update's path cannot be used: {why}"),
            Self::NoVersion => {
                f.write_str("the downloaded update does not say which version it is")
            }
            Self::VersionRefused(version) => {
                write!(
                    f,
                    "the downloaded update is version {version}, which is not newer"
                )
            }
            Self::PerMachine => f.write_str(
                "this copy of Code++ is installed for all users, which cannot update itself yet",
            ),
            Self::Launch(error) => write!(f, "Windows Installer could not be started: {error}"),
        }
    }
}

impl std::error::Error for InstallError {}

/// An installer whose signature has been verified, held open so that it
/// stays the file that was verified.
///
/// Dropping it lets go of the file without installing anything.
#[derive(Debug)]
pub struct VerifiedInstaller {
    /// The handle the bytes were verified through. Open sharing reads
    /// only, for as long as this value lives; [`Self::launch`] hands a
    /// copy of it to the installer's process.
    file: File,
    /// The file's own full path, with no link in it, in the plain form
    /// (`C:\…`).
    path: PathBuf,
    version: String,
}

/// Verify the installer at `path` and keep hold of it.
///
/// In this order, each step only if the one before it passed:
///
/// 1. The file is opened sharing reads only, and must be a plain file
///    on a disk, at most [`MAX_INSTALLER_BYTES`] long.
/// 2. Its bytes, read through that handle, must carry one of
///    `signatures` as `public_key`'s Ed25519 signature. The signatures
///    are the ones the feed states (DESIGN.md §11.4); the feed is not
///    trusted, and a signature that verifies needs no trust.
/// 3. The file's own path is taken from the handle, so a path that led
///    here through a link is replaced by where the file is.
/// 4. The package states its version, and `accept_version` accepts it.
///    The caller refuses a version that is not newer than the running
///    one: a signature has no expiry and names no version.
///
/// # Errors
///
/// [`InstallError`] says which step refused. The file is let go of.
pub fn verify(
    path: &Path,
    public_key: &[u8; PUBLIC_KEY_LEN],
    signatures: &[[u8; SIGNATURE_LEN]],
    accept_version: impl FnOnce(&str) -> bool,
) -> Result<VerifiedInstaller, InstallError> {
    verify_capped(
        path,
        public_key,
        signatures,
        accept_version,
        MAX_INSTALLER_BYTES,
    )
}

/// [`verify`], with the size cap as a parameter so that a test reaches
/// it with a small file.
fn verify_capped(
    path: &Path,
    public_key: &[u8; PUBLIC_KEY_LEN],
    signatures: &[[u8; SIGNATURE_LEN]],
    accept_version: impl FnOnce(&str) -> bool,
    max_bytes: u64,
) -> Result<VerifiedInstaller, InstallError> {
    let file = open_held(path).map_err(InstallError::Open)?;
    // The type first: a device has no metadata to ask for.
    if !is_disk_file(&file) {
        return Err(InstallError::NotAFile);
    }
    let metadata = file.metadata().map_err(InstallError::Read)?;
    if !metadata.file_type().is_file() {
        return Err(InstallError::NotAFile);
    }
    if metadata.len() > max_bytes {
        return Err(InstallError::TooLarge);
    }
    // Nothing can make the file grow while it is held, but the bound is
    // on the read itself all the same.
    let mut bytes = Vec::new();
    // Reserved up front and fallibly: a file near the cap must not end
    // the process where it could end the update.
    let expected = usize::try_from(metadata.len()).map_err(|_| InstallError::TooLarge)?;
    bytes
        .try_reserve_exact(expected.saturating_add(1))
        .map_err(|_| InstallError::Read(std::io::ErrorKind::OutOfMemory.into()))?;
    (&file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(InstallError::Read)?;
    if bytes.len() as u64 > max_bytes {
        return Err(InstallError::TooLarge);
    }
    if !signatures
        .iter()
        .any(|signature| verifies(public_key, signature, &bytes))
    {
        return Err(InstallError::BadSignature);
    }
    drop(bytes);

    let path = own_plain_path(&file)?;
    let version = product_version(&path).ok_or(InstallError::NoVersion)?;
    if !accept_version(&version) {
        return Err(InstallError::VersionRefused(version));
    }
    Ok(VerifiedInstaller {
        file,
        path,
        version,
    })
}

/// Open `path` sharing reads only, without following a link at its
/// end, trying again for a moment while something else has it open
/// (see [`OPEN_ATTEMPTS`]).
fn open_held(path: &Path) -> std::io::Result<File> {
    let mut attempt = 1;
    loop {
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(SHARE_READ_ONLY)
            .custom_flags(OPEN_REPARSE_POINT)
            .open(path);
        match opened {
            Err(error)
                if error.raw_os_error() == Some(SHARING_VIOLATION) && attempt < OPEN_ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(OPEN_RETRY_WAIT);
            }
            other => return other,
        }
    }
}

/// Whether `file` is a file on a disk, and not a pipe or a device.
fn is_disk_file(file: &File) -> bool {
    // SAFETY: the handle is `file`'s own, open for the call, and
    // `GetFileType` only reads its type.
    unsafe { GetFileType(HANDLE(file.as_raw_handle())) == FILE_TYPE_DISK }
}

/// The full path of the file `file` has open, as the volume has it
/// (every link resolved), in the plain form `C:\…`, checked to lead
/// back to the same file.
///
/// Windows hands the path back in the verbatim form, `\\?\C:\…`, which
/// is not known to be taken by `msiexec.exe`. Dropping the prefix gives
/// a path Windows normalises as it reads it (a name's trailing dot or
/// space is dropped), so the plain path is opened and its file compared
/// with the one held: it is handed on only if it is that very file.
fn own_plain_path(file: &File) -> Result<PathBuf, InstallError> {
    let handle = HANDLE(file.as_raw_handle());
    let mut units = [0u16; 1024];
    // SAFETY: the handle is `file`'s own and `units` is passed with its
    // true length. Flags of 0 ask for the normalised name with a drive
    // letter.
    let len =
        unsafe { GetFinalPathNameByHandleW(handle, &mut units, GETFINALPATHNAMEBYHANDLE_FLAGS(0)) }
            as usize;
    if len == 0 || len >= units.len() {
        return Err(InstallError::Path("Windows did not give it"));
    }
    let plain = plain_form(&units[..len])?;
    let path = PathBuf::from(OsString::from_wide(plain));

    let again = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_ONLY)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(&path)
        .map_err(|_| InstallError::Path("it does not lead back to the file"))?;
    let held = file_identity(file);
    if held.is_none() || held != file_identity(&again) {
        return Err(InstallError::Path("it does not lead back to the file"));
    }
    Ok(path)
}

/// `\\?\C:\…` as `C:\…`, refusing every other form and a path too long
/// to be taken as written.
fn plain_form(verbatim: &[u16]) -> Result<&[u16], InstallError> {
    const PREFIX: [u16; 4] = [0x5C, 0x5C, 0x3F, 0x5C]; // \\?\
    let Some(rest) = verbatim.strip_prefix(&PREFIX) else {
        return Err(InstallError::Path("it is not on a local drive"));
    };
    let on_a_drive = matches!(
        rest,
        [letter, 0x3A, 0x5C, ..] if u8::try_from(*letter).is_ok_and(|l| l.is_ascii_alphabetic())
    );
    if !on_a_drive {
        return Err(InstallError::Path("it is not on a local drive"));
    }
    if rest.len() > MAX_PLAIN_PATH_UNITS {
        return Err(InstallError::Path("it is too long"));
    }
    // A second colon names a stream of a file (`x.msi:other`), which is
    // not the file an installer is.
    if rest[2..].contains(&0x3A) {
        return Err(InstallError::Path("it names a stream of a file"));
    }
    Ok(rest)
}

/// What makes a file itself on this machine: its volume's serial number
/// and its identifier on the volume, the 128-bit one where the file
/// system has it (`ReFS` numbers files past 64 bits) and the 64-bit
/// index otherwise.
fn file_identity(file: &File) -> Option<(u64, [u8; 16])> {
    let handle = HANDLE(file.as_raw_handle());
    let mut wide = FILE_ID_INFO::default();
    let size = u32::try_from(size_of::<FILE_ID_INFO>()).ok()?;
    // SAFETY: the handle is `file`'s own, and `wide` is a local of the
    // type `FileIdInfo` fills in, passed with its size.
    let asked =
        unsafe { GetFileInformationByHandleEx(handle, FileIdInfo, (&raw mut wide).cast(), size) };
    if asked.is_ok() {
        return Some((wide.VolumeSerialNumber, wide.FileId.Identifier));
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is `file`'s own and `info` is a local of the
    // type the call fills in.
    unsafe { GetFileInformationByHandle(handle, &raw mut info) }.ok()?;
    let mut identifier = [0u8; 16];
    identifier[..4].copy_from_slice(&info.nFileIndexLow.to_le_bytes());
    identifier[4..8].copy_from_slice(&info.nFileIndexHigh.to_le_bytes());
    Some((u64::from(info.dwVolumeSerialNumber), identifier))
}

impl VerifiedInstaller {
    /// The version the package states.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The file's own full path, as it is handed to Windows Installer.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start Windows Installer on the verified file, to install over
    /// the Code++ of `kind` in `install_dir`, and return its process
    /// id.
    ///
    /// The installer's process inherits the handle the file was
    /// verified through, so the file cannot be written, replaced or
    /// renamed until that process ends, whether or not Code++ is still
    /// running. It exists, with the handle, by the time this returns,
    /// and the caller may then exit; it should, since Windows Installer
    /// cannot replace an executable that is running.
    ///
    /// `install_dir` is the running executable's own folder, which the
    /// installer is told to use: the package installs into its default
    /// folder otherwise, wherever the installed version is
    /// (DESIGN.md §9.2). Windows Installer shows its progress and, at
    /// the end, says that it is done (`/qb+`), since nothing starts
    /// Code++ again afterwards.
    ///
    /// # Errors
    ///
    /// [`InstallError::PerMachine`] while [`PER_MACHINE_UPDATES`] is
    /// false, [`InstallError::Path`] for a folder that cannot be put in
    /// a command line, [`InstallError::Launch`] if the process could
    /// not be started.
    pub fn launch(self, kind: InstallKind, install_dir: &Path) -> Result<u32, InstallError> {
        if kind == InstallKind::PerMachine && !PER_MACHINE_UPDATES {
            return Err(InstallError::PerMachine);
        }
        let system = system_directory().ok_or(InstallError::Path("no system folder"))?;
        let mut command = installer_command(&system, &self.path, kind, install_dir)?;
        let child = spawn_holding(&self.file, &mut command).map_err(InstallError::Launch)?;
        Ok(child.id())
    }
}

/// Windows' system folder, `C:\Windows\System32`.
fn system_directory() -> Option<PathBuf> {
    let mut units = [0u16; 512];
    // SAFETY: `units` is passed with its true length.
    let len = unsafe { GetSystemDirectoryW(Some(&mut units)) } as usize;
    (len != 0 && len < units.len()).then(|| PathBuf::from(OsString::from_wide(&units[..len])))
}

/// The `msiexec.exe` command that installs `package` over the Code++ of
/// `kind` in `install_dir`.
///
/// Every part is Code++'s own: the executable by its full path in
/// `system`, fixed options, the path [`verify`] took from the file's
/// handle, and the running executable's folder. Nothing from the feed
/// or the registry is in it. An installation for one account is told to
/// stay one (`MSIINSTALLPERUSER=1 ALLUSERS=2`), or the package would
/// install for the whole machine beside it.
fn installer_command(
    system: &Path,
    package: &Path,
    kind: InstallKind,
    install_dir: &Path,
) -> Result<Command, InstallError> {
    let mut command = Command::new(system.join("msiexec.exe"));
    command
        .arg("/i")
        .raw_arg(quoted(package)?)
        .arg("/qb+")
        .raw_arg(format!("INSTALLFOLDER={}", quoted(install_dir)?));
    if kind == InstallKind::PerUser {
        command.arg("MSIINSTALLPERUSER=1").arg("ALLUSERS=2");
    }
    // Not the folder Code++ was started from, which the installer's
    // process would otherwise keep open, the installation's own
    // included.
    command
        .current_dir(system)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

/// `path` between double quotes, for `msiexec.exe`'s command line.
///
/// A Windows path holds no `"`, so the quotes need no escaping; one
/// that did, or a control character, is refused all the same. Trailing
/// separators are dropped, since a `\` before the closing quote reads
/// as an escape to some command-line parsers.
fn quoted(path: &Path) -> Result<String, InstallError> {
    let text = path
        .to_str()
        .ok_or(InstallError::Path("it is not valid Unicode"))?
        .trim_end_matches(['\\', '/']);
    if text.is_empty() || text.ends_with(':') {
        return Err(InstallError::Path("it names no folder below a drive"));
    }
    if text.chars().any(|c| c == '"' || c.is_control()) {
        return Err(InstallError::Path("it holds a character it may not"));
    }
    Ok(format!("\"{text}\""))
}

/// Start `command` so that its process inherits `file`'s handle, and
/// with it whatever the handle holds still.
///
/// The handle is marked inheritable and the process created with handle
/// inheritance, which is how `std` creates every process. The new
/// process never uses the handle; having it is the point. A process
/// started by another thread at the same moment may inherit it too, and
/// then holds the file a little longer, which harms nothing.
fn spawn_holding(file: &File, command: &mut Command) -> std::io::Result<Child> {
    // SAFETY: the handle is `file`'s own, open for the call.
    unsafe {
        SetHandleInformation(
            HANDLE(file.as_raw_handle()),
            HANDLE_FLAG_INHERIT.0,
            HANDLE_FLAG_INHERIT,
        )
    }
    .map_err(std::io::Error::other)?;
    command.spawn()
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;
    use crate::msi_package::make_test_package;

    /// A key pair for the tests, from a fixed seed.
    fn key_pair() -> (SigningKey, [u8; PUBLIC_KEY_LEN]) {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let public = signing.verifying_key().to_bytes();
        (signing, public)
    }

    /// A package stating `version`, at `dir\name`, and its signature.
    fn signed_package(dir: &Path, name: &str, version: &str) -> (PathBuf, [u8; SIGNATURE_LEN]) {
        let path = dir.join(name);
        make_test_package(&path, &[("ProductVersion", version)]);
        let signature = key_pair().0.sign(&std::fs::read(&path).unwrap()).to_bytes();
        (path, signature)
    }

    /// Whether `path` can be opened for writing right now.
    fn can_write(path: &Path) -> bool {
        std::fs::OpenOptions::new().write(true).open(path).is_ok()
    }

    #[test]
    fn a_signed_package_is_verified_and_held() {
        let dir = tempfile::tempdir().unwrap();
        let (path, signature) = signed_package(dir.path(), "update.msi", "0.2.1");
        let (_, public) = key_pair();
        let installer = verify(&path, &public, &[signature], |version| version == "0.2.1").unwrap();
        assert_eq!(installer.version(), "0.2.1");
        assert_eq!(
            std::fs::canonicalize(&path).unwrap(),
            std::fs::canonicalize(installer.path()).unwrap()
        );
        assert!(
            !installer
                .path()
                .as_os_str()
                .to_string_lossy()
                .starts_with(r"\\?\"),
            "the plain form is what msiexec is given"
        );
        // Held: nothing can write, delete or rename it, or rename its folder.
        assert!(!can_write(&path));
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, dir.path().join("moved.msi")).is_err());
        let elsewhere = dir.path().with_extension("moved");
        assert!(std::fs::rename(dir.path(), &elsewhere).is_err());
        // It can still be read, which is what the installer does.
        assert!(std::fs::read(&path).is_ok());
        drop(installer);
        assert!(can_write(&path), "dropping it lets go of the file");
    }

    #[test]
    fn the_signature_may_be_any_one_the_feed_states() {
        let dir = tempfile::tempdir().unwrap();
        let (path, signature) = signed_package(dir.path(), "update.msi", "0.2.1");
        let (_, public) = key_pair();
        let others = [[1u8; SIGNATURE_LEN], signature, [2u8; SIGNATURE_LEN]];
        assert!(verify(&path, &public, &others, |_| true).is_ok());
    }

    #[test]
    fn a_file_the_key_did_not_sign_is_refused_and_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let (path, signature) = signed_package(dir.path(), "update.msi", "0.2.1");
        let (_, public) = key_pair();
        let asked = std::cell::Cell::new(false);
        let accept = |_: &str| {
            asked.set(true);
            true
        };

        // No signature at all, and signatures of something else.
        assert!(matches!(
            verify(&path, &public, &[], accept),
            Err(InstallError::BadSignature)
        ));
        let (other, other_signature) = signed_package(dir.path(), "other.msi", "0.2.2");
        assert!(matches!(
            verify(
                &path,
                &public,
                &[other_signature, [0u8; SIGNATURE_LEN]],
                accept
            ),
            Err(InstallError::BadSignature)
        ));
        // Another key's signature of this very file.
        let stranger = SigningKey::from_bytes(&[8u8; 32]);
        let strangers = stranger.sign(&std::fs::read(&path).unwrap()).to_bytes();
        assert!(matches!(
            verify(&path, &public, &[strangers], accept),
            Err(InstallError::BadSignature)
        ));
        // The right signature, and one byte of the file changed since.
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            verify(&path, &public, &[signature], accept),
            Err(InstallError::BadSignature)
        ));
        assert!(
            !asked.get(),
            "a file that is not verified is not asked for its version"
        );
        assert!(can_write(&path), "a refused file is let go of");
        assert!(can_write(&other));
    }

    #[test]
    fn the_version_is_asked_of_the_package_and_may_be_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (_, public) = key_pair();
        let (path, signature) = signed_package(dir.path(), "update.msi", "0.2.1");
        let seen = std::cell::RefCell::new(String::new());
        let refused = verify(&path, &public, &[signature], |version| {
            version.clone_into(&mut seen.borrow_mut());
            false
        });
        assert!(matches!(refused, Err(InstallError::VersionRefused(v)) if v == "0.2.1"));
        assert_eq!(*seen.borrow(), "0.2.1");
        assert!(can_write(&path));

        // A signed file that is no package states no version.
        let text = dir.path().join("text.msi");
        std::fs::write(&text, b"signed, and not a package").unwrap();
        let signature = key_pair().0.sign(&std::fs::read(&text).unwrap()).to_bytes();
        assert!(matches!(
            verify(&text, &public, &[signature], |_| true),
            Err(InstallError::NoVersion)
        ));
        assert!(can_write(&text));
    }

    #[test]
    fn a_file_over_the_cap_is_refused_and_one_at_it_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let (path, signature) = signed_package(dir.path(), "update.msi", "0.2.1");
        let (_, public) = key_pair();
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(matches!(
            verify_capped(&path, &public, &[signature], |_| true, len - 1),
            Err(InstallError::TooLarge)
        ));
        assert!(verify_capped(&path, &public, &[signature], |_| true, len).is_ok());
    }

    #[test]
    fn a_file_something_else_can_write_is_not_taken() {
        let dir = tempfile::tempdir().unwrap();
        let (path, signature) = signed_package(dir.path(), "update.msi", "0.2.1");
        let (_, public) = key_pair();
        // Another program has it open for writing, sharing everything,
        // and keeps it: the open is tried for a moment and then refused.
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(7)
            .open(&path)
            .unwrap();
        let before = std::time::Instant::now();
        let refused = verify(&path, &public, &[signature], |_| true);
        assert!(
            matches!(&refused, Err(InstallError::Open(e)) if e.raw_os_error() == Some(SHARING_VIOLATION)),
            "{refused:?}"
        );
        assert!(before.elapsed() >= OPEN_RETRY_WAIT * (OPEN_ATTEMPTS - 1));
        drop(writer);
        assert!(verify(&path, &public, &[signature], |_| true).is_ok());
    }

    #[test]
    fn a_file_a_scanner_has_open_for_a_moment_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let (path, signature) = signed_package(dir.path(), "update.msi", "0.2.1");
        let (_, public) = key_pair();
        let scanner = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(7)
            .open(&path)
            .unwrap();
        let (opened_tx, opened_rx) = std::sync::mpsc::channel::<()>();
        let letting_go = std::thread::spawn(move || {
            opened_tx.send(()).unwrap();
            std::thread::sleep(OPEN_RETRY_WAIT * 2);
            drop(scanner);
        });
        opened_rx.recv().unwrap();
        let installer = verify(&path, &public, &[signature], |_| true);
        letting_go.join().unwrap();
        assert!(installer.is_ok(), "{installer:?}");
    }

    #[test]
    fn what_is_not_a_plain_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (_, public) = key_pair();
        assert!(matches!(
            verify(&dir.path().join("missing.msi"), &public, &[], |_| true),
            Err(InstallError::Open(_))
        ));
        assert!(matches!(
            verify(dir.path(), &public, &[], |_| true),
            Err(InstallError::Open(_) | InstallError::NotAFile)
        ));
        assert!(matches!(
            verify(Path::new(r"\\.\NUL"), &public, &[], |_| true),
            Err(InstallError::NotAFile)
        ));
    }

    #[test]
    fn a_path_through_a_junction_is_replaced_by_the_files_own() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let (path, signature) = signed_package(&real, "update.msi", "0.2.1");
        let link = dir.path().join("link");
        let made = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(made.success(), "mklink /J");
        let (_, public) = key_pair();
        let installer = verify(&link.join("update.msi"), &public, &[signature], |_| true).unwrap();
        let handed = installer.path().to_str().unwrap().to_lowercase();
        assert!(handed.ends_with(r"\real\update.msi"), "{handed}");
        assert!(!handed.contains(r"\link\"), "{handed}");
        drop(installer);
        // Remove the junction itself before the folder is cleaned up.
        std::fs::remove_dir(&link).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn only_a_plain_drive_path_is_handed_on() {
        fn units(text: &str) -> Vec<u16> {
            text.encode_utf16().collect()
        }
        assert_eq!(
            plain_form(&units(r"\\?\C:\Temp\Update-1\x.msi")).unwrap(),
            units(r"C:\Temp\Update-1\x.msi")
        );
        for refused in [
            r"C:\Temp\x.msi",
            r"\\?\UNC\server\share\x.msi",
            r"\\?\Volume{0a0a0a0a-0000-0000-0000-000000000000}\x.msi",
            r"\\?\GLOBALROOT\Device\x.msi",
            r"\\?\C:",
            r"\\?\1:\x.msi",
            r"\\?\\C:\x.msi",
            r"\\.\C:\x.msi",
            // A stream of a file is not the file.
            r"\\?\C:\Temp\x.msi:evil",
            r"\\?\C:\Temp\x.msi::$DATA",
            r"\\?\C:\Temp:x\y.msi",
            "",
        ] {
            assert!(plain_form(&units(refused)).is_err(), "{refused}");
        }
        let longest = format!(r"\\?\C:\{}", "a".repeat(MAX_PLAIN_PATH_UNITS - 3));
        assert!(plain_form(&units(&longest)).is_ok());
        let too_long = format!(r"\\?\C:\{}", "a".repeat(MAX_PLAIN_PATH_UNITS - 2));
        assert!(plain_form(&units(&too_long)).is_err());
    }

    #[test]
    fn the_kind_is_the_root_that_records_this_folder() {
        let dir = Path::new(r"C:\Program Files\Code++");
        let recorded = |machine: Option<&'static str>, user: Option<&'static str>| {
            move |kind: InstallKind| match kind {
                InstallKind::PerMachine => machine.map(str::to_owned),
                InstallKind::PerUser => user.map(str::to_owned),
            }
        };
        // The installer records the folder with a trailing separator.
        assert_eq!(
            kind_from(dir, recorded(Some(r"C:\Program Files\Code++\"), None)),
            Some(InstallKind::PerMachine)
        );
        assert_eq!(
            kind_from(dir, recorded(None, Some(r"c:\program files\code++"))),
            Some(InstallKind::PerUser)
        );
        // Both name it: the machine's record is asked first, so a record
        // planted for the account does not make it an account's.
        assert_eq!(
            kind_from(
                dir,
                recorded(
                    Some(r"C:\Program Files\Code++\"),
                    Some(r"C:\Program Files\Code++\")
                )
            ),
            Some(InstallKind::PerMachine)
        );
        // A record of another folder is not this installation's.
        assert_eq!(
            kind_from(
                dir,
                recorded(
                    Some(r"D:\Apps\Code++\"),
                    Some(r"C:\Users\x\AppData\Local\Programs\Code++\")
                )
            ),
            None
        );
        assert_eq!(
            kind_from(
                dir,
                recorded(Some(r"D:\Apps\Code++\"), Some(r"C:\Program Files\Code++\"))
            ),
            Some(InstallKind::PerUser)
        );
        for not_this in [
            "",
            r"\",
            r"C:\Program Files",
            r"C:\Program Files\Code++\bin",
            r"C:\Program Files\Code++ 2",
            r"C:\PROGRA~1\Code++",
        ] {
            assert_eq!(
                kind_from(dir, recorded(Some(not_this), Some(not_this))),
                None,
                "{not_this}"
            );
        }
        // No record at all: a copy from the zip, or a build tree.
        assert_eq!(kind_from(dir, recorded(None, None)), None);
        // A copy that is not on a local drive is no installation, though
        // a record names its folder.
        for elsewhere in [
            r"\\server\share\Code++",
            r"\\?\C:\Program Files\Code++",
            r"Code++",
            r"\Program Files\Code++",
            r"1:\Code++",
            "",
        ] {
            assert_eq!(
                kind_from(
                    Path::new(elsewhere),
                    recorded(Some(elsewhere), Some(elsewhere))
                ),
                None,
                "{elsewhere}"
            );
        }
    }

    #[test]
    fn the_command_is_built_from_code_pluspluss_own_parts() {
        let system = Path::new(r"C:\Windows\System32");
        let package = Path::new(r"C:\Users\x y\AppData\Local\Temp\Update-1\codepp 0.2.1.msi");
        let command = installer_command(
            system,
            package,
            InstallKind::PerUser,
            Path::new(r"C:\Users\x y\AppData\Local\Programs\Code++\"),
        )
        .unwrap();
        assert_eq!(
            command.get_program(),
            system.join("msiexec.exe").as_os_str()
        );
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "/i",
                r#""C:\Users\x y\AppData\Local\Temp\Update-1\codepp 0.2.1.msi""#,
                "/qb+",
                r#"INSTALLFOLDER="C:\Users\x y\AppData\Local\Programs\Code++""#,
                "MSIINSTALLPERUSER=1",
                "ALLUSERS=2",
            ]
        );
        assert_eq!(command.get_current_dir(), Some(system));

        // For the whole machine, nothing says "for this account".
        let command = installer_command(
            system,
            package,
            InstallKind::PerMachine,
            Path::new(r"C:\Program Files\Code++"),
        )
        .unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args.len(), 4, "{args:?}");
        assert_eq!(args[3], r#"INSTALLFOLDER="C:\Program Files\Code++""#);
    }

    #[test]
    fn a_path_that_cannot_be_quoted_is_refused() {
        assert_eq!(quoted(Path::new(r"C:\a b\")).unwrap(), r#""C:\a b""#);
        assert_eq!(quoted(Path::new(r"C:\a\\\")).unwrap(), r#""C:\a""#);
        for refused in [r"", r"\", r"C:\", r"C:", "C:\\a\"b", "C:\\a\nb", "C:\\a\0b"] {
            assert!(quoted(Path::new(refused)).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn an_installation_for_the_whole_machine_is_not_started() {
        let dir = tempfile::tempdir().unwrap();
        let (path, signature) = signed_package(dir.path(), "update.msi", "0.2.1");
        let (_, public) = key_pair();
        let installer = verify(&path, &public, &[signature], |_| true).unwrap();
        // This test goes when the gate is lifted.
        const { assert!(!PER_MACHINE_UPDATES) };
        assert!(matches!(
            installer.launch(
                InstallKind::PerMachine,
                Path::new(r"C:\Program Files\Code++")
            ),
            Err(InstallError::PerMachine)
        ));
        assert!(can_write(&path), "and the file is let go of");
    }

    /// The hand-over that `launch` rests on, with a harmless process in
    /// the installer's place: `cmd /c pause`, which waits for its input
    /// to end.
    #[test]
    fn the_started_process_holds_the_file_after_the_handle_here_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (path, signature) = signed_package(dir.path(), "update.msi", "0.2.1");
        let (_, public) = key_pair();
        let installer = verify(&path, &public, &[signature], |_| true).unwrap();

        let system = system_directory().unwrap();
        let mut command = Command::new(system.join("cmd.exe"));
        command
            .args(["/d", "/c", "pause"])
            .current_dir(&system)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_holding(&installer.file, &mut command).unwrap();
        drop(installer);

        // Code++'s own handle is closed, and the file is held still.
        let refused = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(32), "a sharing violation");
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, dir.path().join("moved.msi")).is_err());
        assert!(std::fs::read(&path).is_ok());

        // The process ends, and with it the hold. A process another
        // test starts at this moment inherits the handle too, and may
        // outlive this one by an instant, so the check allows for it.
        drop(child.stdin.take());
        child.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !can_write(&path) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(can_write(&path));
    }
}
