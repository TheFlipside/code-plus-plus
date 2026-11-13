//! The binding to `WinSparkle.dll`, the Windows updater library
//! (DESIGN.md §11).
//!
//! `WinSparkle` checks a feed for a new version, shows the windows that
//! offer it, downloads it and verifies its signature. Code++ decides
//! when a check is due, keeps the library's state, and runs the
//! installer itself; this module is the seam between the two.
//!
//! **Loaded lazily, by full path, and never before first paint.**
//! `codepp.exe` does not import the DLL. [`WinSparkle::start`] loads it
//! with `LOAD_LIBRARY_SEARCH_SYSTEM32` and no other search flag, so the
//! DLL itself comes from the path given and everything it imports from
//! System32. It refuses a DLL whose version is not the one this binding
//! was written against, since another version has another interface.
//! That check guards against a mismatched file, not a hostile one: a
//! version resource is anyone's to write, and the file is read for it
//! before it is loaded, not at the same moment. Whoever can write the
//! installation's folder can replace `codepp.exe` itself (DESIGN.md
//! §11.2), and the packaging pins the DLL it ships by hash (§11.8).
//! Nothing here runs unless a caller starts it, and the caller does so
//! from a worker thread: the load can take a second on a file the
//! antivirus has not seen, and `win_sparkle_init` removes the last
//! download's folder on the thread that calls it (both measured,
//! DESIGN.md §11.11).
//!
//! **Once per process, and never unloaded.** The library's callbacks
//! carry no context, so they find the one started instance through a
//! process-wide cell. The DLL starts threads of its own, and unmapping
//! a library under its threads is what crashed the host with plugins
//! (DESIGN.md §6.4), so it stays mapped until the process ends.
//!
//! **No registry.** The library's state goes through a [`ConfigStore`]
//! the caller supplies, by `win_sparkle_set_config_methods`. With the
//! methods replaced the library reads and writes four values and
//! creates no registry key (measured).
//!
//! **Callbacks arrive on the library's threads**, never the caller's
//! (its header, and measured). Each is turned into an [`Event`] on a
//! channel and touches nothing else, which is the marshaling rule of
//! DESIGN.md §5.4.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crossbeam_channel::Sender;
use windows::core::{HSTRING, PCSTR, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

/// The DLL's file name, beside `codepp.exe` in an installation.
pub const DLL_NAME: &str = "WinSparkle.dll";

/// The version of the DLL this binding was written against, as the
/// four numbers of its version resource. [`WinSparkle::start`] refuses
/// any other.
pub const EXPECTED_VERSION: [u16; 4] = [0, 9, 4, 0];

/// The name of the value in which the library keeps its last download's
/// folder, which it removes at its next start.
const UPDATE_TEMP_DIR: &str = "UpdateTempDir";

/// The most UTF-16 units read from a string the library hands over.
/// Windows' longest path.
const MAX_WIDE_UNITS: usize = 32_767;

/// What the library reports, each from a thread of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A check found a newer version. The library shows its window.
    FoundUpdate,
    /// A check found nothing newer.
    NoUpdate,
    /// A check or a download failed. The library has shown its own
    /// message if the check was one with a window.
    Error,
    /// The user dismissed an offered update.
    Cancelled,
    /// A download is complete and its signature verified by the
    /// library: the installer at this path is the caller's to verify
    /// again and run (DESIGN.md §11.4). The file is not locked, and
    /// anything the user runs can still rewrite it.
    InstallerReady(PathBuf),
    /// The library asks the application to quit, which it does after
    /// every [`Event::InstallerReady`], straight away.
    ShutdownRequested,
}

/// Where the library's state is kept: the caller's replacement for the
/// registry. Called from the thread that starts the library and from
/// the library's own threads, so an implementation locks for itself.
pub trait ConfigStore: Send + Sync {
    /// The value kept under `name`, if any.
    fn read(&self, name: &str) -> Option<String>;
    /// Keep `value` under `name`.
    fn write(&self, name: &str, value: &str);
    /// Forget the value kept under `name`.
    fn delete(&self, name: &str);
}

/// What the library is told about the application and its feed.
#[derive(Clone, Copy, Debug)]
pub struct Setup<'a> {
    /// The feed's URL.
    pub feed_url: &'a str,
    /// The Ed25519 public key downloads are verified against, as base64.
    pub public_key: &'a str,
    /// The company name, which with the application's makes the user
    /// agent and the default registry path the library does not use.
    pub company: &'a str,
    /// The application's name, shown in the library's windows.
    pub app: &'a str,
    /// The running version, which the feed's is compared with.
    pub version: &'a str,
}

/// Why the library could not be started.
#[derive(Debug)]
pub enum StartError {
    /// It was started before in this process.
    AlreadyStarted,
    /// The path is not a full path on a local drive: one that is not
    /// absolute would be searched for, and one on a network share would
    /// load code from another machine.
    NotAbsolute,
    /// The DLL's version resource could not be read.
    NoVersion,
    /// The DLL is not the version this binding was written against.
    WrongVersion([u16; 4]),
    /// `LoadLibraryExW` failed.
    Load(windows::core::Error),
    /// The DLL lacks a function this binding calls.
    MissingFunction(&'static str),
    /// A text of the [`Setup`] holds a NUL.
    InvalidSetup(&'static str),
    /// The library refused the public key.
    BadPublicKey,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyStarted => write!(f, "WinSparkle was already started"),
            Self::NotAbsolute => write!(
                f,
                "the path to WinSparkle.dll is not a full path on a local drive"
            ),
            Self::NoVersion => write!(f, "WinSparkle.dll has no readable version"),
            Self::WrongVersion(v) => write!(
                f,
                "WinSparkle.dll is version {}.{}.{}.{}, not the {}.{}.{}.{} this build expects",
                v[0],
                v[1],
                v[2],
                v[3],
                EXPECTED_VERSION[0],
                EXPECTED_VERSION[1],
                EXPECTED_VERSION[2],
                EXPECTED_VERSION[3]
            ),
            Self::Load(e) => write!(f, "loading WinSparkle.dll failed: {e}"),
            Self::MissingFunction(name) => write!(f, "WinSparkle.dll has no {name}"),
            Self::InvalidSetup(what) => write!(f, "the updater's {what} holds a NUL"),
            Self::BadPublicKey => write!(f, "WinSparkle refused the public key"),
        }
    }
}

impl std::error::Error for StartError {}

type VoidCallback = extern "C" fn();
type IntCallback = extern "C" fn() -> c_int;
type InstallerCallback = extern "C" fn(*const u16) -> c_int;

/// `win_sparkle_config_methods_t`, as the library's header declares it.
#[repr(C)]
struct ConfigMethods {
    read: Option<extern "C" fn(*const c_char, *mut u16, usize, *mut c_void) -> c_int>,
    write: Option<extern "C" fn(*const c_char, *const u16, *mut c_void)>,
    delete: Option<extern "C" fn(*const c_char, *mut c_void)>,
    user_data: *mut c_void,
}

/// The functions of the DLL this binding calls, all `__cdecl`.
struct Functions {
    init: unsafe extern "C" fn(),
    cleanup: unsafe extern "C" fn(),
    set_appcast_url: unsafe extern "C" fn(*const c_char),
    set_eddsa_public_key: unsafe extern "C" fn(*const c_char) -> c_int,
    set_app_details: unsafe extern "C" fn(*const u16, *const u16, *const u16),
    set_config_methods: unsafe extern "C" fn(*mut ConfigMethods),
    set_automatic_check_for_updates: unsafe extern "C" fn(c_int),
    set_error_callback: unsafe extern "C" fn(Option<VoidCallback>),
    set_can_shutdown_callback: unsafe extern "C" fn(Option<IntCallback>),
    set_shutdown_request_callback: unsafe extern "C" fn(Option<VoidCallback>),
    set_did_find_update_callback: unsafe extern "C" fn(Option<VoidCallback>),
    set_did_not_find_update_callback: unsafe extern "C" fn(Option<VoidCallback>),
    set_update_cancelled_callback: unsafe extern "C" fn(Option<VoidCallback>),
    set_user_run_installer_callback: unsafe extern "C" fn(Option<InstallerCallback>),
    check_update_with_ui: unsafe extern "C" fn(),
    check_update_without_ui: unsafe extern "C" fn(),
}

/// The started library. There is at most one in a process.
pub struct WinSparkle {
    functions: Functions,
    store: Box<dyn ConfigStore>,
    events: Sender<Event>,
    /// The user's temporary folder, which a kept download folder must
    /// be a direct child of to be handed back to the library.
    temp_dir: PathBuf,
}

/// The one started instance, which the library's callbacks read.
static INSTANCE: OnceLock<WinSparkle> = OnceLock::new();

impl WinSparkle {
    /// Load the DLL at `dll`, configure it from `setup`, and start it
    /// with automatic checks off: it checks only when asked.
    ///
    /// Call this from a worker thread, after first paint. The library
    /// keeps its state in `store` and reports on `events`.
    ///
    /// # Errors
    ///
    /// A [`StartError`] when the DLL is not at a full path on a local
    /// drive, is not [`EXPECTED_VERSION`], cannot be loaded or lacks a
    /// function, when `setup` holds a NUL or a key the library refuses,
    /// or when the library was started before. Every error but
    /// [`StartError::AlreadyStarted`] leaves the library not started and
    /// the process free to try again, though the DLL may by then be
    /// mapped, and stays so.
    pub fn start(
        dll: &Path,
        setup: &Setup<'_>,
        store: Box<dyn ConfigStore>,
        events: Sender<Event>,
    ) -> Result<&'static Self, StartError> {
        if INSTANCE.get().is_some() {
            return Err(StartError::AlreadyStarted);
        }
        if !is_local_full_path(dll) {
            return Err(StartError::NotAbsolute);
        }
        let feed_url =
            CString::new(setup.feed_url).map_err(|_| StartError::InvalidSetup("feed URL"))?;
        let public_key =
            CString::new(setup.public_key).map_err(|_| StartError::InvalidSetup("public key"))?;
        for (text, what) in [
            (setup.company, "company name"),
            (setup.app, "name"),
            (setup.version, "version"),
        ] {
            if text.contains('\0') {
                return Err(StartError::InvalidSetup(what));
            }
        }
        let version = file_version(dll).ok_or(StartError::NoVersion)?;
        if version != EXPECTED_VERSION {
            return Err(StartError::WrongVersion(version));
        }
        let functions = load(dll)?;
        // Before the cell is taken, so that a refused key leaves the
        // process free to start the library with another. The key is
        // kept in the library's own settings, not through the
        // configuration methods, which are not replaced yet.
        // SAFETY: a NUL-terminated string that outlives the call, to
        // the function of that name and signature in the checked DLL.
        if unsafe { (functions.set_eddsa_public_key)(public_key.as_ptr()) } == 0 {
            return Err(StartError::BadPublicKey);
        }
        let mut installed = false;
        let this = INSTANCE.get_or_init(|| {
            installed = true;
            Self {
                functions,
                store,
                events,
                temp_dir: std::env::temp_dir(),
            }
        });
        if !installed {
            return Err(StartError::AlreadyStarted);
        }

        let f = &this.functions;
        let company = HSTRING::from(setup.company);
        let app = HSTRING::from(setup.app);
        let app_version = HSTRING::from(setup.version);
        let mut methods = ConfigMethods {
            read: Some(config_read),
            write: Some(config_write),
            delete: Some(config_delete),
            user_data: std::ptr::null_mut(),
        };
        // SAFETY: every pointer passed is to a NUL-terminated string or
        // a struct that lives across its call, and the library copies
        // what it keeps (its settings hold `std::string`s and a copy of
        // the methods struct; read from `settings.cpp` of 0.9.4). The
        // callbacks are `extern "C"` functions of this module that
        // never unwind. The functions were resolved by name from the
        // DLL whose version was checked, and are called with the
        // signatures its header declares.
        unsafe {
            (f.set_config_methods)(&raw mut methods);
            (f.set_appcast_url)(feed_url.as_ptr());
            (f.set_app_details)(company.as_ptr(), app.as_ptr(), app_version.as_ptr());
            // Before `init`: with this off the library neither checks
            // at start nor asks the user whether it may (measured).
            (f.set_automatic_check_for_updates)(0);
            (f.set_error_callback)(Some(on_error));
            (f.set_can_shutdown_callback)(Some(on_can_shutdown));
            (f.set_shutdown_request_callback)(Some(on_shutdown_request));
            (f.set_did_find_update_callback)(Some(on_found));
            (f.set_did_not_find_update_callback)(Some(on_not_found));
            (f.set_update_cancelled_callback)(Some(on_cancelled));
            (f.set_user_run_installer_callback)(Some(on_run_installer));
            (f.init)();
        }
        Ok(this)
    }

    /// Check the feed without a window. One opens only if a newer
    /// version is found. Returns at once; the result is an [`Event`].
    pub fn check_in_background(&self) {
        // SAFETY: the library is started; the function takes nothing.
        unsafe { (self.functions.check_update_without_ui)() }
    }

    /// Check the feed with the library's progress window, as the user
    /// asked for from the menu. Returns at once.
    pub fn check_with_window(&self) {
        // SAFETY: as in `check_in_background`.
        unsafe { (self.functions.check_update_with_ui)() }
    }

    /// Cancel what the library has in progress and stop its threads.
    /// Called once, as the application quits, and last: the library
    /// says nothing about a check asked for after it, and nothing here
    /// stops one.
    pub fn cleanup(&self) {
        // SAFETY: as in `check_in_background`.
        unsafe { (self.functions.cleanup)() }
    }

    /// Report `event`. False when nobody is listening any more, which
    /// is an application on its way out.
    fn send(&self, event: Event) -> bool {
        self.events.send(event).is_ok()
    }
}

/// The DLL beside the running executable, if there is one: where an
/// installation that can update itself keeps it.
#[must_use]
pub fn dll_beside_exe() -> Option<PathBuf> {
    let dll = std::env::current_exe().ok()?.parent()?.join(DLL_NAME);
    dll.is_file().then_some(dll)
}

/// Whether `path` is a full path on a local drive, `C:\…` or its
/// verbatim form `\\?\C:\…`, with no NUL in it. A relative path would
/// be searched for, a path on a share (`\\server\…`) would load code
/// from another machine, and Windows would read a path with a NUL only
/// up to it.
fn is_local_full_path(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};
    let on_a_drive = matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    );
    on_a_drive && path.is_absolute() && !path.as_os_str().encode_wide().any(|unit| unit == 0)
}

/// Whether `value` is a download folder as the library names one: a
/// direct child of `temp_dir` called `Update-` and a GUID.
///
/// The library removes the folder its `UpdateTempDir` value names when
/// it starts, and its own check is only that the path begins with the
/// temporary folder and `Update-`. The value lives in a file a user can
/// edit, so Code++ hands back only a value of exactly the shape the
/// library writes: the temporary folder's own text, one separator, and
/// the name. The folder's text is compared without regard to ASCII
/// case, as Windows compares paths, and as text, so no `.` or `..`
/// component and no doubled separator passes for it. A temporary
/// folder that is not an absolute path names no folder to compare with.
///
/// This is about the value's text. Whether a folder of that name is a
/// folder and not a link is asked where the value is handed over.
#[must_use]
pub fn is_update_temp_dir(value: &str, temp_dir: &Path) -> bool {
    const SEPARATORS: [char; 2] = ['\\', '/'];
    let Some(temp) = temp_dir.to_str() else {
        return false;
    };
    let temp = temp.trim_end_matches(SEPARATORS);
    let Some((parent, name)) = value.rsplit_once(SEPARATORS) else {
        return false;
    };
    temp_dir.is_absolute()
        && parent.eq_ignore_ascii_case(temp)
        && name.strip_prefix("Update-").is_some_and(is_guid)
}

/// Whether `text` is a GUID as the library prints one:
/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` in hex digits.
fn is_guid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The signature every `VS_FIXEDFILEINFO` begins with.
const VS_FFI_SIGNATURE: u32 = 0xFEEF_04BD;

/// The four numbers of `path`'s version resource.
fn file_version(path: &Path) -> Option<[u16; 4]> {
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: `wide` is NUL-terminated and outlives the calls; the
    // buffer is as large as the size the system asked for; and
    // `VerQueryValueW` for `\` yields a pointer into that buffer to a
    // `VS_FIXEDFILEINFO`, checked for its length before it is read.
    unsafe {
        let size = GetFileVersionInfoSizeW(&wide, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&wide, None, size, data.as_mut_ptr().cast()).ok()?;
        let mut info: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            windows::core::w!("\\"),
            &raw mut info,
            &raw mut len,
        )
        .as_bool()
            || info.is_null()
            || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let fixed = info.cast::<VS_FIXEDFILEINFO>().read_unaligned();
        if fixed.dwSignature != VS_FFI_SIGNATURE {
            return None;
        }
        let (ms, ls) = (fixed.dwFileVersionMS, fixed.dwFileVersionLS);
        Some([
            (ms >> 16) as u16,
            (ms & 0xFFFF) as u16,
            (ls >> 16) as u16,
            (ls & 0xFFFF) as u16,
        ])
    }
}

/// Load the DLL and resolve every function this binding calls.
fn load(dll: &Path) -> Result<Functions, StartError> {
    let wide = HSTRING::from(dll.as_os_str());
    // SAFETY: `wide` is a NUL-terminated absolute path that outlives
    // the call. `LOAD_LIBRARY_SEARCH_SYSTEM32` alone makes the loader
    // take the DLL's imports from System32 and nowhere else.
    let module =
        unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map_err(StartError::Load)?;

    /// Resolve `$name` in `module` as a function of type `$ty`.
    macro_rules! resolve {
        ($name:literal, $ty:ty) => {{
            // SAFETY: the name is NUL-terminated, and the address is
            // that of the function of this name in the DLL whose
            // version was checked, declared in its header with the
            // signature `$ty` states.
            unsafe {
                let address = GetProcAddress(module, PCSTR(concat!($name, "\0").as_ptr()))
                    .ok_or(StartError::MissingFunction($name))?;
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(address)
            }
        }};
    }
    let _: HMODULE = module;
    Ok(Functions {
        init: resolve!("win_sparkle_init", unsafe extern "C" fn()),
        cleanup: resolve!("win_sparkle_cleanup", unsafe extern "C" fn()),
        set_appcast_url: resolve!(
            "win_sparkle_set_appcast_url",
            unsafe extern "C" fn(*const c_char)
        ),
        set_eddsa_public_key: resolve!(
            "win_sparkle_set_eddsa_public_key",
            unsafe extern "C" fn(*const c_char) -> c_int
        ),
        set_app_details: resolve!(
            "win_sparkle_set_app_details",
            unsafe extern "C" fn(*const u16, *const u16, *const u16)
        ),
        set_config_methods: resolve!(
            "win_sparkle_set_config_methods",
            unsafe extern "C" fn(*mut ConfigMethods)
        ),
        set_automatic_check_for_updates: resolve!(
            "win_sparkle_set_automatic_check_for_updates",
            unsafe extern "C" fn(c_int)
        ),
        set_error_callback: resolve!(
            "win_sparkle_set_error_callback",
            unsafe extern "C" fn(Option<VoidCallback>)
        ),
        set_can_shutdown_callback: resolve!(
            "win_sparkle_set_can_shutdown_callback",
            unsafe extern "C" fn(Option<IntCallback>)
        ),
        set_shutdown_request_callback: resolve!(
            "win_sparkle_set_shutdown_request_callback",
            unsafe extern "C" fn(Option<VoidCallback>)
        ),
        set_did_find_update_callback: resolve!(
            "win_sparkle_set_did_find_update_callback",
            unsafe extern "C" fn(Option<VoidCallback>)
        ),
        set_did_not_find_update_callback: resolve!(
            "win_sparkle_set_did_not_find_update_callback",
            unsafe extern "C" fn(Option<VoidCallback>)
        ),
        set_update_cancelled_callback: resolve!(
            "win_sparkle_set_update_cancelled_callback",
            unsafe extern "C" fn(Option<VoidCallback>)
        ),
        set_user_run_installer_callback: resolve!(
            "win_sparkle_set_user_run_installer_callback",
            unsafe extern "C" fn(Option<InstallerCallback>)
        ),
        check_update_with_ui: resolve!("win_sparkle_check_update_with_ui", unsafe extern "C" fn()),
        check_update_without_ui: resolve!(
            "win_sparkle_check_update_without_ui",
            unsafe extern "C" fn()
        ),
    })
}

/// Run `body` for a callback from the library and give `fallback` if it
/// panics: a panic must not unwind into the library's frames.
fn at_boundary<T>(fallback: T, body: impl FnOnce(&'static WinSparkle) -> T) -> T {
    let Some(this) = INSTANCE.get() else {
        return fallback;
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(this))).unwrap_or_else(|_| {
        tracing::error!("panic in a WinSparkle callback");
        fallback
    })
}

/// The text of a NUL-terminated C string the library passes as a name.
///
/// # Safety
///
/// `name` is null or points at a NUL-terminated string.
unsafe fn name_of<'a>(name: *const c_char) -> Option<&'a str> {
    if name.is_null() {
        return None;
    }
    // SAFETY: not null, and NUL-terminated by the caller's contract.
    unsafe { CStr::from_ptr(name) }.to_str().ok()
}

/// The text of a NUL-terminated wide string the library passes, if it
/// is valid UTF-16 and ends within [`MAX_WIDE_UNITS`].
///
/// # Safety
///
/// `text` is null or points at a NUL-terminated wide string.
unsafe fn wide_text(text: *const u16) -> Option<String> {
    if text.is_null() {
        return None;
    }
    let mut len = 0usize;
    // SAFETY: reads stop at the string's NUL, by the caller's contract,
    // or at the cap.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
        if len > MAX_WIDE_UNITS {
            return None;
        }
    }
    // SAFETY: `len` units before the NUL were just read.
    String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) }).ok()
}

/// The value kept under `name`, as the library may have it.
///
/// Every value but one is handed over as kept. `UpdateTempDir` names a
/// folder the library removes when it starts, so it is handed over only
/// when it is a download folder of the library's own shape
/// ([`is_update_temp_dir`]) that is there as a plain folder; a link or a
/// junction under that name would have the library remove through it or
/// around it, and which was not measured. Anything else is dropped from
/// the store, and the library, told there is no such value, removes
/// nothing on its account.
///
/// A link put there between this check and the library's removal is not
/// caught, and cannot be from here. Whoever can do that can remove the
/// user's files without Code++'s help; the check is against a state
/// file that is stale, synced from elsewhere or edited by hand.
fn read_for_library(store: &dyn ConfigStore, temp_dir: &Path, name: &str) -> Option<String> {
    let value = store.read(name)?;
    if name == UPDATE_TEMP_DIR
        && !(is_update_temp_dir(&value, temp_dir) && is_plain_folder(Path::new(&value)))
    {
        tracing::warn!("dropped a kept download folder that is not one of the updater's");
        store.delete(name);
        return None;
    }
    Some(value)
}

/// Whether `path` is a folder that is there and is not a link, a
/// junction or any other reparse point. The path itself is looked at,
/// not what a link at it leads to.
fn is_plain_folder(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    /// `FILE_ATTRIBUTE_REPARSE_POINT`.
    const REPARSE_POINT: u32 = 0x400;
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_dir() && meta.file_attributes() & REPARSE_POINT == 0)
}

/// Copy `value` and its NUL into `buf`, which holds `len` UTF-16 units.
/// False, with nothing written, when `buf` is null or `value` and its
/// NUL do not fit: a value is not handed over in part.
///
/// # Safety
///
/// `buf` is null or valid for writes of `len` UTF-16 units.
unsafe fn copy_to_wide_buffer(value: &str, buf: *mut u16, len: usize) -> bool {
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    if buf.is_null() || wide.len() > len {
        return false;
    }
    // SAFETY: `buf` holds `len` units and `wide.len() <= len`.
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), buf, wide.len()) };
    true
}

extern "C" fn config_read(
    name: *const c_char,
    buf: *mut u16,
    len: usize,
    _user: *mut c_void,
) -> c_int {
    at_boundary(0, |this| {
        // SAFETY: the library passes a NUL-terminated name.
        let Some(name) = (unsafe { name_of(name) }) else {
            return 0;
        };
        let Some(value) = read_for_library(this.store.as_ref(), &this.temp_dir, name) else {
            return 0;
        };
        // SAFETY: the library passes a buffer of `len` UTF-16 units
        // (`settings.cpp` of 0.9.4 passes its array's element count).
        c_int::from(unsafe { copy_to_wide_buffer(&value, buf, len) })
    })
}

extern "C" fn config_write(name: *const c_char, value: *const u16, _user: *mut c_void) {
    at_boundary((), |this| {
        // SAFETY: the library passes NUL-terminated strings.
        let (Some(name), Some(value)) = (unsafe { name_of(name) }, unsafe { wide_text(value) })
        else {
            return;
        };
        this.store.write(name, &value);
    });
}

extern "C" fn config_delete(name: *const c_char, _user: *mut c_void) {
    at_boundary((), |this| {
        // SAFETY: the library passes a NUL-terminated name.
        if let Some(name) = unsafe { name_of(name) } {
            this.store.delete(name);
        }
    });
}

extern "C" fn on_error() {
    at_boundary((), |this| {
        this.send(Event::Error);
    });
}

extern "C" fn on_found() {
    at_boundary((), |this| {
        this.send(Event::FoundUpdate);
    });
}

extern "C" fn on_not_found() {
    at_boundary((), |this| {
        this.send(Event::NoUpdate);
    });
}

extern "C" fn on_cancelled() {
    at_boundary((), |this| {
        this.send(Event::Cancelled);
    });
}

extern "C" fn on_shutdown_request() {
    at_boundary((), |this| {
        this.send(Event::ShutdownRequested);
    });
}

/// The library asks whether the application can quit before it runs an
/// installer. Always yes: Code++'s own quit asks about unsaved work
/// when it comes to it, and may then be declined.
extern "C" fn on_can_shutdown() -> c_int {
    1
}

/// The library hands over a downloaded installer. Answering 1 says the
/// application takes it from here, so the library does not run it.
extern "C" fn on_run_installer(path: *const u16) -> c_int {
    // The library's `WINSPARKLE_RETURN_ERROR`.
    const RETURN_ERROR: c_int = -1;
    at_boundary(RETURN_ERROR, |this| {
        // SAFETY: the library passes a NUL-terminated path.
        let Some(path) = (unsafe { wide_text(path) }) else {
            return RETURN_ERROR;
        };
        // "Handled" only if someone has it: with nobody listening, the
        // library is told the hand-over failed rather than left to
        // think the application runs an installer it never heard of.
        if this.send(Event::InstallerReady(PathBuf::from(path))) {
            1
        } else {
            RETURN_ERROR
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const TEMP: &str = r"C:\Users\someone\AppData\Local\Temp";
    const GUID: &str = "57316f78-ad89-4269-93e6-42375d9645cb";

    #[test]
    fn a_folder_as_the_library_names_one_is_an_update_temp_dir() {
        let temp = Path::new(TEMP);
        assert!(is_update_temp_dir(&format!(r"{TEMP}\Update-{GUID}"), temp));
        assert!(
            is_update_temp_dir(&format!(r"{}\Update-{GUID}", TEMP.to_uppercase()), temp),
            "the temporary folder's case does not matter"
        );
        assert!(is_update_temp_dir(
            &format!(r"{}\Update-{}", TEMP.to_lowercase(), GUID.to_uppercase()),
            temp
        ));
        assert!(is_update_temp_dir(
            &format!(r"{TEMP}\Update-{GUID}"),
            Path::new(&format!(r"{TEMP}\"))
        ));
        assert!(
            is_update_temp_dir(&format!("{TEMP}/Update-{GUID}"), temp),
            "either separator before the name"
        );
        let not_ascii = r"C:\Users\zoë\AppData\Local\Temp";
        assert!(is_update_temp_dir(
            &format!(r"{not_ascii}\Update-{GUID}"),
            Path::new(not_ascii)
        ));
    }

    #[test]
    fn any_other_folder_is_not() {
        let temp = Path::new(TEMP);
        for value in [
            String::new(),
            TEMP.to_owned(),
            format!(r"{TEMP}\Update-"),
            format!(r"{TEMP}\Update-victim"),
            format!(r"{TEMP}\Update-{GUID}x"),
            format!(r"{TEMP}\Update-{}", &GUID[1..]),
            format!(r"{TEMP}\Update-{}", GUID.replace('-', "")),
            format!(r"{TEMP}\Update-{}", GUID.replace('a', "g")),
            format!(r"{TEMP}\update-{GUID}"),
            format!(r"{TEMP}\sub\Update-{GUID}"),
            format!(r"{TEMP}\Update-{GUID}\inner"),
            format!(r"{TEMP}\Update-{GUID}\..\..\Documents"),
            format!(r"{TEMP}\..\Temp\Update-{GUID}"),
            format!(r"{TEMP}\.\Update-{GUID}"),
            format!(r"C:\Users\someone\Documents\Update-{GUID}"),
            format!(r"D:{}\Update-{GUID}", &TEMP[2..]),
            format!(r"Update-{GUID}"),
            format!(r"\\server\share\Update-{GUID}"),
            format!(r"{TEMP}\Update-{GUID}:stream"),
            format!(r"{TEMP}\Update-{GUID}."),
            format!(r"{TEMP}\Update-{GUID} "),
            format!(r"{TEMP}\\Update-{GUID}"),
            format!(r"{TEMP}\Update-{GUID}\"),
        ] {
            assert!(!is_update_temp_dir(&value, temp), "{value:?}");
        }
        // A temporary folder that is no absolute path names no folder.
        for relative in ["", "Temp", r"Temp\sub"] {
            assert!(
                !is_update_temp_dir(&format!(r"{relative}\Update-{GUID}"), Path::new(relative)),
                "{relative:?}"
            );
        }
    }

    #[test]
    fn a_guid_is_five_groups_of_hex() {
        assert!(is_guid(GUID));
        assert!(is_guid(&GUID.to_uppercase()));
        for text in [
            "",
            "57316f78",
            "57316f78-ad89-4269-93e6",
            "57316f78-ad89-4269-93e6-42375d9645cb-00",
            "57316f78-ad89-4269-93e6-42375d9645c",
            "57316f78-ad89-4269-93e6-42375d9645cbb",
            "5731-6f78ad89-4269-93e6-42375d9645cb",
            "57316f7g-ad89-4269-93e6-42375d9645cb",
            "57316f78_ad89_4269_93e6_42375d9645cb",
        ] {
            assert!(!is_guid(text), "{text:?}");
        }
    }

    #[test]
    fn a_wide_string_is_read_to_its_nul_and_no_further() {
        let wide: Vec<u16> = "C:\\a b\\ü.msi\0tail".encode_utf16().collect();
        // SAFETY: NUL-terminated within the vector.
        assert_eq!(
            unsafe { wide_text(wide.as_ptr()) }.as_deref(),
            Some("C:\\a b\\ü.msi")
        );
        // SAFETY: a null pointer is allowed.
        assert_eq!(unsafe { wide_text(std::ptr::null()) }, None);
        let lone_surrogate = [0xD800u16, 0];
        // SAFETY: NUL-terminated.
        assert_eq!(unsafe { wide_text(lone_surrogate.as_ptr()) }, None);
        let mut long = vec![u16::from(b'a'); MAX_WIDE_UNITS + 1];
        long.push(0);
        // SAFETY: NUL-terminated.
        assert_eq!(unsafe { wide_text(long.as_ptr()) }, None);
        long.remove(0);
        // SAFETY: NUL-terminated.
        assert_eq!(
            unsafe { wide_text(long.as_ptr()) }.map(|s| s.len()),
            Some(MAX_WIDE_UNITS)
        );
    }

    #[test]
    fn a_dll_that_is_not_there_or_not_absolute_is_refused_before_anything_loads() {
        let (events, _rx) = crossbeam_channel::unbounded();
        let setup = Setup {
            feed_url: "https://example.org/appcast.xml",
            public_key: "GfY7YUA2b3vvIlE+0mmQzGWrS+73SQL/6/68MxGvhvg=",
            company: "Code++",
            app: "Code++",
            version: "0.1.0",
        };
        for not_local in ["WinSparkle.dll", r"\\server\share\WinSparkle.dll"] {
            assert!(
                matches!(
                    WinSparkle::start(
                        Path::new(not_local),
                        &setup,
                        Box::new(MemoryStore::default()),
                        events.clone()
                    ),
                    Err(StartError::NotAbsolute)
                ),
                "{not_local}"
            );
        }
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            WinSparkle::start(
                &dir.path().join("WinSparkle.dll"),
                &setup,
                Box::new(MemoryStore::default()),
                events.clone()
            ),
            Err(StartError::NoVersion)
        ));
        // A DLL of another version: Windows' own kernel32.
        let system = std::env::var_os("SystemRoot").expect("SystemRoot");
        let kernel32 = Path::new(&system).join("System32").join("kernel32.dll");
        assert!(matches!(
            WinSparkle::start(
                &kernel32,
                &setup,
                Box::new(MemoryStore::default()),
                events.clone()
            ),
            Err(StartError::WrongVersion(_))
        ));
        let bad = Setup {
            feed_url: "https://example.org/\0",
            ..setup
        };
        assert!(matches!(
            WinSparkle::start(&kernel32, &bad, Box::new(MemoryStore::default()), events),
            Err(StartError::InvalidSetup("feed URL"))
        ));
    }

    #[test]
    fn only_a_full_path_on_a_local_drive_is_one() {
        for path in [
            r"C:\Program Files\Code++\WinSparkle.dll",
            r"\\?\C:\a\WinSparkle.dll",
            "D:/a/b.dll",
        ] {
            assert!(is_local_full_path(Path::new(path)), "{path}");
        }
        for path in [
            "WinSparkle.dll",
            r".\WinSparkle.dll",
            r"C:WinSparkle.dll",
            r"\WinSparkle.dll",
            r"\\server\share\WinSparkle.dll",
            r"\\?\UNC\server\share\WinSparkle.dll",
            r"\\.\pipe\WinSparkle.dll",
            "",
        ] {
            assert!(!is_local_full_path(Path::new(path)), "{path}");
        }
        let with_nul = std::ffi::OsString::from("C:\\a\\Win\0Sparkle.dll");
        assert!(!is_local_full_path(Path::new(&with_nul)));
    }

    #[test]
    fn a_value_is_copied_whole_or_not_at_all() {
        let mut buf = [0xFFFFu16; 8];
        // SAFETY: each call is given the buffer's true length or less.
        unsafe {
            assert!(
                copy_to_wide_buffer("abc", buf.as_mut_ptr(), 4),
                "an exact fit"
            );
            assert_eq!(buf[..5], [97, 98, 99, 0, 0xFFFF]);
            buf = [0xFFFF; 8];
            assert!(
                !copy_to_wide_buffer("abc", buf.as_mut_ptr(), 3),
                "no room for the NUL"
            );
            assert!(!copy_to_wide_buffer("abc", buf.as_mut_ptr(), 0));
            assert_eq!(buf, [0xFFFF; 8], "nothing was written");
            assert!(copy_to_wide_buffer("", buf.as_mut_ptr(), 1));
            assert_eq!(buf[..2], [0, 0xFFFF]);
            assert!(!copy_to_wide_buffer("abc", std::ptr::null_mut(), 8));
            // A character outside the BMP is two units.
            assert!(!copy_to_wide_buffer("\u{1F600}", buf.as_mut_ptr(), 2));
            assert!(copy_to_wide_buffer("\u{1F600}", buf.as_mut_ptr(), 3));
        }
    }

    /// What the library is handed for `UpdateTempDir`: its own kind of
    /// folder, there and plain, and nothing else. A refused value is
    /// dropped from the store. Every other value is handed over as kept.
    #[test]
    fn only_a_plain_folder_of_the_librarys_own_is_handed_back() {
        let temp = tempfile::tempdir().expect("tempdir");
        let temp_dir = temp.path();
        let own = temp_dir.join(format!("Update-{GUID}"));
        std::fs::create_dir(&own).expect("create");
        let own_text = own.to_str().expect("utf-8").to_owned();
        let store = MemoryStore::default();

        store.write(UPDATE_TEMP_DIR, &own_text);
        assert_eq!(
            read_for_library(&store, temp_dir, UPDATE_TEMP_DIR).as_deref(),
            Some(own_text.as_str())
        );
        assert!(store.read(UPDATE_TEMP_DIR).is_some(), "and kept");

        // A folder of another name, there or not.
        let other = temp_dir.join("Documents");
        std::fs::create_dir(&other).expect("create");
        for refused in [
            other.to_str().expect("utf-8").to_owned(),
            temp_dir
                .join(format!("Update-{}", GUID.replace('5', "6")))
                .to_str()
                .expect("utf-8")
                .to_owned(),
            String::new(),
        ] {
            store.write(UPDATE_TEMP_DIR, &refused);
            assert_eq!(
                read_for_library(&store, temp_dir, UPDATE_TEMP_DIR),
                None,
                "{refused:?}"
            );
            assert_eq!(store.read(UPDATE_TEMP_DIR), None, "dropped: {refused:?}");
        }
        assert!(other.is_dir(), "nothing here removes a folder");

        // A file under the library's own kind of name.
        let file = temp_dir.join(format!("Update-{}", GUID.replace('5', "7")));
        std::fs::write(&file, b"x").expect("write");
        store.write(UPDATE_TEMP_DIR, file.to_str().expect("utf-8"));
        assert_eq!(read_for_library(&store, temp_dir, UPDATE_TEMP_DIR), None);

        // A junction under it, leading to a folder that is not the
        // library's. `mklink /J` needs no privilege.
        let link = temp_dir.join(format!("Update-{}", GUID.replace('5', "8")));
        let made = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&other)
            .output()
            .expect("run mklink");
        assert!(made.status.success(), "mklink /J failed: {made:?}");
        assert!(link.is_dir(), "the junction leads to a folder");
        store.write(UPDATE_TEMP_DIR, link.to_str().expect("utf-8"));
        assert_eq!(
            read_for_library(&store, temp_dir, UPDATE_TEMP_DIR),
            None,
            "a junction is not a plain folder"
        );
        assert_eq!(store.read(UPDATE_TEMP_DIR), None);
        std::fs::remove_dir(&link).expect("remove the junction");

        // Any other value is the library's business.
        store.write("SkipThisVersion", &other.to_string_lossy());
        assert_eq!(
            read_for_library(&store, temp_dir, "SkipThisVersion"),
            Some(other.to_string_lossy().into_owned())
        );
        assert_eq!(read_for_library(&store, temp_dir, "Missing"), None);
    }

    /// A store in memory, shared with the test so it can look inside.
    #[derive(Clone, Default)]
    struct MemoryStore(Arc<Mutex<HashMap<String, String>>>);

    impl ConfigStore for MemoryStore {
        fn read(&self, name: &str) -> Option<String> {
            self.0.lock().expect("lock").get(name).cloned()
        }
        fn write(&self, name: &str, value: &str) {
            self.0
                .lock()
                .expect("lock")
                .insert(name.to_owned(), value.to_owned());
        }
        fn delete(&self, name: &str) {
            self.0.lock().expect("lock").remove(name);
        }
    }

    /// Serve `body` as the answer to every request on a local port, and
    /// return the port.
    fn serve(body: String) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        port
    }

    /// The real DLL, driven through this binding: it loads under the
    /// search flag, takes the setup, keeps its state in the store and
    /// nowhere else, hands back only a download folder of its own
    /// shape, and reports a feed with nothing newer as [`Event::NoUpdate`]
    /// from a thread that is not the caller's.
    ///
    /// Opt-in, since the DLL is not in the tree: set
    /// `CODEPP_WINSPARKLE_DLL` to the x64 `WinSparkle.dll` of 0.9.4 and
    /// run with `--ignored`. No window opens: the feed offers nothing
    /// newer, and the check is one without a window.
    #[test]
    #[ignore = "needs CODEPP_WINSPARKLE_DLL, the path of WinSparkle.dll 0.9.4"]
    fn the_real_dll_checks_a_feed_through_this_binding() {
        let dll = PathBuf::from(
            std::env::var_os("CODEPP_WINSPARKLE_DLL").expect("CODEPP_WINSPARKLE_DLL is not set"),
        );
        let port = serve(
            "<?xml version=\"1.0\"?><rss version=\"2.0\" \
             xmlns:sparkle=\"http://www.andymatuschak.org/xml-namespaces/sparkle\"><channel>\
             <item><sparkle:version>0.0.1</sparkle:version>\
             <enclosure url=\"http://127.0.0.1:1/none.msi\" length=\"1\" \
             type=\"application/octet-stream\"/></item></channel></rss>"
                .to_owned(),
        );
        let store = MemoryStore::default();
        // A kept folder that is not one of the library's must not reach it.
        let victim = tempfile::tempdir().expect("tempdir");
        store.write(UPDATE_TEMP_DIR, &victim.path().to_string_lossy());
        let (events, rx) = crossbeam_channel::unbounded();
        let feed = format!("http://127.0.0.1:{port}/appcast.xml");
        // A key the library refuses leaves the process free to start it.
        assert!(matches!(
            WinSparkle::start(
                &dll,
                &Setup {
                    feed_url: &feed,
                    public_key: "not a key",
                    company: "Code++ tests",
                    app: "Code++ binding test",
                    version: "0.1.0",
                },
                Box::new(MemoryStore::default()),
                events.clone(),
            ),
            Err(StartError::BadPublicKey)
        ));
        let ws = WinSparkle::start(
            &dll,
            &Setup {
                feed_url: &feed,
                public_key: "GfY7YUA2b3vvIlE+0mmQzGWrS+73SQL/6/68MxGvhvg=",
                company: "Code++ tests",
                app: "Code++ binding test",
                version: "0.1.0",
            },
            Box::new(store.clone()),
            events.clone(),
        )
        .expect("start");
        assert!(victim.path().is_dir(), "the folder was left alone");
        assert_eq!(
            store.read(UPDATE_TEMP_DIR),
            None,
            "and the value naming it dropped"
        );
        assert_eq!(store.read("CheckForUpdates").as_deref(), Some("0"));

        ws.check_in_background();
        let event = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("an event within 30 s");
        assert_eq!(event, Event::NoUpdate);
        assert!(
            store.read("LastCheckTime").is_some(),
            "the check's time went to the store"
        );
        assert!(matches!(
            WinSparkle::start(
                &dll,
                &Setup {
                    feed_url: &feed,
                    public_key: "",
                    company: "",
                    app: "",
                    version: "",
                },
                Box::new(MemoryStore::default()),
                events,
            ),
            Err(StartError::AlreadyStarted)
        ));
        ws.cleanup();
        // The library's default home for its state is the registry key
        // of the company and the application. There is none.
        let key = std::process::Command::new("reg")
            .args(["query", r"HKCU\Software\Code++ tests"])
            .output()
            .expect("run reg");
        assert!(
            !key.status.success(),
            "the library wrote to the registry: {}",
            String::from_utf8_lossy(&key.stdout)
        );
    }
}
