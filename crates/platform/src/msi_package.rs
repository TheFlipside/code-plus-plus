//! Reading a Windows Installer package's version (DESIGN.md §11.4).
//!
//! A signature on an update has no expiry and names no version, so the
//! updater asks the package itself which version it installs, and
//! refuses one that is not newer than the running Code++. The answer is
//! the package's `ProductVersion` property, the one Windows Installer
//! compares.
//!
//! The package is read through Windows Installer's own database
//! functions, opened read-only. That runs nothing of the package: a
//! database open reads its tables and no more. It is still a parser
//! handed a file, so the caller reads a package's version only *after*
//! it has verified the package's signature.
//!
//! `msi.dll` is not imported: it is loaded from System32 the first time
//! a version is read, so a start that installs nothing never maps it
//! (DESIGN.md §11.2).

use std::path::Path;
use std::sync::OnceLock;

use windows::core::{s, w, HSTRING, PCWSTR};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

/// The size of the buffer a `ProductVersion` is read into, in UTF-16
/// units, its NUL included: a longer version is not read. Windows
/// Installer's own form, `255.255.65535`, is thirteen units.
const VERSION_BUFFER_UNITS: u32 = 64;

/// The longest `ProductVersion` read, in UTF-16 units. Windows
/// Installer's own form, `255.255.65535`, is thirteen.
#[cfg(test)]
const MAX_VERSION_UNITS: usize = VERSION_BUFFER_UNITS as usize - 1;

/// Windows Installer's handle to a database, view or record.
type MsiHandle = u32;
type OpenDatabaseFn = unsafe extern "system" fn(PCWSTR, PCWSTR, *mut MsiHandle) -> u32;
type OpenViewFn = unsafe extern "system" fn(MsiHandle, PCWSTR, *mut MsiHandle) -> u32;
type ViewExecuteFn = unsafe extern "system" fn(MsiHandle, MsiHandle) -> u32;
type ViewFetchFn = unsafe extern "system" fn(MsiHandle, *mut MsiHandle) -> u32;
type RecordGetStringFn = unsafe extern "system" fn(MsiHandle, u32, *mut u16, *mut u32) -> u32;
type CloseHandleFn = unsafe extern "system" fn(MsiHandle) -> u32;

/// The entry points of `msi.dll` this module calls, each with the
/// signature `msiquery.h` declares.
struct Msi {
    open_database: OpenDatabaseFn,
    open_view: OpenViewFn,
    view_execute: ViewExecuteFn,
    view_fetch: ViewFetchFn,
    record_get_string: RecordGetStringFn,
    close_handle: CloseHandleFn,
}

/// [`Msi`], resolved once for the process, or `None` where Windows
/// Installer is not to be found.
fn msi() -> Option<&'static Msi> {
    static MSI: OnceLock<Option<Msi>> = OnceLock::new();
    MSI.get_or_init(resolve_msi).as_ref()
}

fn resolve_msi() -> Option<Msi> {
    type Farproc = unsafe extern "system" fn() -> isize;
    // SAFETY: `LOAD_LIBRARY_SEARCH_SYSTEM32` confines the load to the
    // system directory. Each entry point is transmuted, from the
    // function pointer `GetProcAddress` returns, to the signature its
    // SDK header declares: the same size, and the only use made of it.
    unsafe {
        let dll = LoadLibraryExW(w!("msi.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
        Some(Msi {
            open_database: std::mem::transmute::<Farproc, OpenDatabaseFn>(GetProcAddress(
                dll,
                s!("MsiOpenDatabaseW"),
            )?),
            open_view: std::mem::transmute::<Farproc, OpenViewFn>(GetProcAddress(
                dll,
                s!("MsiDatabaseOpenViewW"),
            )?),
            view_execute: std::mem::transmute::<Farproc, ViewExecuteFn>(GetProcAddress(
                dll,
                s!("MsiViewExecute"),
            )?),
            view_fetch: std::mem::transmute::<Farproc, ViewFetchFn>(GetProcAddress(
                dll,
                s!("MsiViewFetch"),
            )?),
            record_get_string: std::mem::transmute::<Farproc, RecordGetStringFn>(GetProcAddress(
                dll,
                s!("MsiRecordGetStringW"),
            )?),
            close_handle: std::mem::transmute::<Farproc, CloseHandleFn>(GetProcAddress(
                dll,
                s!("MsiCloseHandle"),
            )?),
        })
    }
}

/// A Windows Installer handle, closed when dropped.
struct Handle {
    raw: MsiHandle,
    close: CloseHandleFn,
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `raw` is a handle Windows Installer returned, and
        // this is the only place that closes it.
        unsafe {
            (self.close)(self.raw);
        }
    }
}

/// The `ProductVersion` of the package at `path`, or `None` if the file
/// is not a package Windows Installer can read, has no such property,
/// or states one longer than any version is.
///
/// The package is opened read-only, through its path. A caller that
/// holds the file open sharing reads only can still read it this way
/// (measured), and is then reading the file it holds: nothing can
/// replace a file, or rename a folder above it, while it is open so
/// (DESIGN.md §11.11).
#[must_use]
pub fn product_version(path: &Path) -> Option<String> {
    let api = msi()?;
    let wide = HSTRING::from(path.as_os_str());
    let mut units = [0u16; VERSION_BUFFER_UNITS as usize];
    // SAFETY: every string handed over is NUL-terminated and outlives
    // its call, every out-pointer is to a local of the type the
    // function writes, and `units` is passed with its true length. A
    // persist argument of NULL is `MSIDBOPEN_READONLY`. Each handle is
    // one Windows Installer returned by the call before, closed by its
    // `Handle` in the reverse of the order they were made.
    let len = unsafe {
        let mut raw = 0;
        if (api.open_database)(PCWSTR(wide.as_ptr()), PCWSTR::null(), &raw mut raw) != 0 {
            return None;
        }
        let database = Handle {
            raw,
            close: api.close_handle,
        };
        let mut raw = 0;
        let query = w!("SELECT `Value` FROM `Property` WHERE `Property` = 'ProductVersion'");
        if (api.open_view)(database.raw, query, &raw mut raw) != 0 {
            return None;
        }
        let view = Handle {
            raw,
            close: api.close_handle,
        };
        if (api.view_execute)(view.raw, 0) != 0 {
            return None;
        }
        let mut raw = 0;
        if (api.view_fetch)(view.raw, &raw mut raw) != 0 {
            return None;
        }
        let record = Handle {
            raw,
            close: api.close_handle,
        };
        // In: the buffer's size with its NUL. Out: the length without.
        let mut len = VERSION_BUFFER_UNITS;
        if (api.record_get_string)(record.raw, 1, units.as_mut_ptr(), &raw mut len) != 0 {
            return None;
        }
        len as usize
    };
    let version = String::from_utf16(units.get(..len)?).ok()?;
    (!version.is_empty()).then_some(version)
}

/// Make a Windows Installer database at `path` whose `Property`
/// table holds `properties`, through `msi.dll` itself.
///
/// For tests, this crate's and, through the `test-support` feature,
/// the shell's. Not part of the crate's interface.
///
/// # Panics
///
/// If Windows Installer refuses any step.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn make_test_package(path: &Path, properties: &[(&str, &str)]) {
    type CommitFn = unsafe extern "system" fn(MsiHandle) -> u32;
    type Farproc = unsafe extern "system" fn() -> isize;
    let api = msi().expect("msi.dll");
    // `MSIDBOPEN_CREATE`: a new database, replacing any file there.
    let create = PCWSTR(std::ptr::without_provenance(3));
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: as in `product_version`; `MsiDatabaseCommit` is
    // resolved with the signature `msiquery.h` declares.
    unsafe {
        let dll = LoadLibraryExW(w!("msi.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).unwrap();
        let commit = std::mem::transmute::<Farproc, CommitFn>(
            GetProcAddress(dll, s!("MsiDatabaseCommit")).unwrap(),
        );
        let mut raw = 0;
        assert_eq!(
            (api.open_database)(PCWSTR(wide.as_ptr()), create, &raw mut raw),
            0
        );
        let database = Handle {
            raw,
            close: api.close_handle,
        };
        let mut statements = vec![
            "CREATE TABLE `Property` (`Property` CHAR(72) NOT NULL, `Value` LONGCHAR NOT NULL \
             LOCALIZABLE PRIMARY KEY `Property`)"
                .to_owned(),
        ];
        for (name, value) in properties {
            statements.push(format!(
                "INSERT INTO `Property` (`Property`, `Value`) VALUES ('{name}', '{value}')"
            ));
        }
        for statement in statements {
            let sql = HSTRING::from(statement.as_str());
            let mut raw = 0;
            assert_eq!(
                (api.open_view)(database.raw, PCWSTR(sql.as_ptr()), &raw mut raw),
                0,
                "{statement}"
            );
            let view = Handle {
                raw,
                close: api.close_handle,
            };
            assert_eq!((api.view_execute)(view.raw, 0), 0, "{statement}");
        }
        assert_eq!(commit(database.raw), 0);
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::fs::OpenOptionsExt;

    use super::*;

    #[test]
    fn a_packages_version_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.msi");
        make_test_package(
            &path,
            &[
                ("ProductName", "Example"),
                ("ProductVersion", "1.22.333"),
                ("Manufacturer", "Example"),
            ],
        );
        assert_eq!(product_version(&path).as_deref(), Some("1.22.333"));
    }

    #[test]
    fn a_package_held_open_denying_writes_is_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.msi");
        make_test_package(&path, &[("ProductVersion", "0.2.1")]);
        // `FILE_SHARE_READ` and nothing else, as the updater holds the
        // installer it has verified.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        assert_eq!(product_version(&path).as_deref(), Some("0.2.1"));
        // Reading it did not loosen the hold,
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        // and left nothing of its own open: with the hold gone the
        // file can be written, and deleted.
        drop(held);
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_ok());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn what_is_not_a_package_with_a_version_has_none() {
        let dir = tempfile::tempdir().unwrap();
        // No file at all.
        assert_eq!(product_version(&dir.path().join("missing.msi")), None);
        // A file that is not a database.
        let text = dir.path().join("text.msi");
        std::fs::write(&text, b"not a package").unwrap();
        assert_eq!(product_version(&text), None);
        let empty = dir.path().join("empty.msi");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(product_version(&empty), None);
        // A database without the property, and one where it is empty.
        let without = dir.path().join("without.msi");
        make_test_package(&without, &[("ProductName", "Example")]);
        assert_eq!(product_version(&without), None);
        // A version longer than any version is.
        let long = dir.path().join("long.msi");
        make_test_package(
            &long,
            &[("ProductVersion", &"1".repeat(MAX_VERSION_UNITS + 1))],
        );
        assert_eq!(product_version(&long), None);
        // One at the longest is read.
        let longest = dir.path().join("longest.msi");
        let version = "1".repeat(MAX_VERSION_UNITS);
        make_test_package(&longest, &[("ProductVersion", &version)]);
        assert_eq!(product_version(&longest), Some(version));
    }
}
