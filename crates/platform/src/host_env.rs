//! Host-environment queries: install directory, executable path,
//! Windows release version. Surfaced to the plugin host via
//! `HostServices` so plugin messages
//! `NPPM_GETNPPDIRECTORY`, `NPPM_GETNPPFULLFILEPATH`, and
//! `NPPM_GETWINDOWSVERSION` can answer without having to know about
//! the underlying OS APIs.

use std::path::PathBuf;

/// Full path of the running executable. `None` if `current_exe()`
/// fails — the canonical Linux failure mode is a denied
/// `/proc/self/exe` (containerised CI runners), and on macOS /
/// Windows the call should always succeed for an unprivileged
/// user-launched binary.
#[must_use]
pub fn program_path() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// Directory containing the running executable — i.e. the parent
/// of [`program_path`]. `None` propagates the same failure modes as
/// `program_path` (and additionally returns `None` if the resolved
/// executable somehow has no parent component, which can't happen
/// on a real OS but is documented as defensive contract).
pub fn program_dir() -> Option<PathBuf> {
    program_path()?.parent().map(PathBuf::from)
}

/// Notepad++'s `winVer` enum value for the running OS. The enum is
/// re-derived here from the canonical Notepad++ header so plugins
/// gating on `>= WV_WIN10` see exactly the same numeric values
/// they'd see in N++. Returns `WV_WIN10` (16) on non-Windows
/// targets and on the rare Windows path where `RtlGetVersion`
/// reports an unrecognised major.minor.build triple — defensible
/// for plugin gating because every modern feature plugins probe
/// for ships in Win10+.
#[cfg(target_os = "windows")]
#[must_use]
pub fn windows_version_npp() -> i32 {
    // Mirror Notepad++'s `winVer` enum from `Common.h`. Only the
    // values plugins typically gate on are listed; the older
    // entries (NT4, 2000, XP, Vista, 7, 8/8.1) round to the closest
    // documented value, and unknown future values fall back to
    // WV_WIN10. Plugin gating like `if (winVer >= WV_WIN10)` is the
    // common shape, so the floor is the safest fallback.
    const WV_UNKNOWN: i32 = 0;
    const WV_WS2003: i32 = 5;
    const WV_VISTA: i32 = 9;
    const WV_WIN7: i32 = 11;
    const WV_WIN8: i32 = 12;
    const WV_WIN81: i32 = 13;
    const WV_WIN10: i32 = 16;
    const WV_WIN11: i32 = 17;

    let Some((major, minor, build)) = read_rtl_get_version() else {
        return WV_WIN10;
    };

    // Microsoft's modern versioning: every release after Win10 RTM
    // keeps `dwMajor=10`, only the build number changes. Win11
    // build floor is 22000 (publicly documented in Microsoft's
    // "Windows 11, version 21H2" minimum-spec page).
    match (major, minor) {
        (10, 0) => {
            if build >= 22000 {
                WV_WIN11
            } else {
                WV_WIN10
            }
        }
        (6, 3) => WV_WIN81,
        (6, 2) => WV_WIN8,
        (6, 1) => WV_WIN7,
        (6, 0) => WV_VISTA,
        (5, 2) => WV_WS2003,
        (5, 1 | 0) => WV_UNKNOWN,
        _ => WV_WIN10,
    }
}

/// Non-Windows fallback: report `WV_WIN10` (16). The plugin host
/// only dispatches `NPPM_GETWINDOWSVERSION` on Windows in practice
/// (loading happens via `LoadLibraryW`), so this branch is here for
/// build correctness on the cross-platform CI runners and for
/// future GTK / Cocoa hosts that still want a deterministic
/// answer.
#[cfg(not(target_os = "windows"))]
#[must_use]
pub fn windows_version_npp() -> i32 {
    16 // WV_WIN10
}

/// Probe `RtlGetVersion` for the running kernel's
/// `(major, minor, build)`. `GetVersionExW` is documented as
/// lying ("compatibility shim") on Win8.1+ unless the binary
/// declares the right manifest entries; `RtlGetVersion` is the
/// documented escape hatch every Windows app uses.
#[cfg(target_os = "windows")]
fn read_rtl_get_version() -> Option<(u32, u32, u32)> {
    use windows::core::s;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

    // SAFETY: `GetModuleHandleA(s!("ntdll.dll"))` returns a non-null
    // handle on every supported Windows release; `ntdll.dll` is
    // always loaded into every Win32 process. `GetProcAddress` for
    // the documented `RtlGetVersion` symbol returns a non-null
    // function pointer on every Windows release back to NT 4.
    unsafe {
        // `OSVERSIONINFOEXW` mirrors Microsoft's Windows.h struct.
        // `repr(C)` so the layout matches kernel32's expectation.
        #[repr(C)]
        struct OsVersionInfoExW {
            dw_size: u32,
            dw_major: u32,
            dw_minor: u32,
            dw_build: u32,
            dw_platform_id: u32,
            sz_csd_version: [u16; 128],
            w_service_pack_major: u16,
            w_service_pack_minor: u16,
            w_suite_mask: u16,
            w_product_type: u8,
            w_reserved: u8,
        }

        // `RtlGetVersion(PRTL_OSVERSIONINFOEXW)` returns NTSTATUS;
        // on success, fills the struct with the *real* kernel
        // version (no compatibility shim).
        //
        // The transmute below changes `GetProcAddress`'s declared
        // return type (`Option<unsafe extern "system" fn() -> isize>`
        // — a placeholder shape the `windows` crate uses because no
        // single signature fits every imported function) into the
        // documented signature for `RtlGetVersion`. The kernel
        // contract is what governs the actual call, not the C
        // declaration we used to retrieve the pointer; transmuting
        // here is the standard pattern every Windows app uses to
        // type-erase exported symbols.
        type RtlGetVersionFn = unsafe extern "system" fn(*mut OsVersionInfoExW) -> i32;

        let ntdll = GetModuleHandleA(s!("ntdll.dll")).ok()?;
        let proc = GetProcAddress(ntdll, s!("RtlGetVersion"))?;
        let rtl_get_version: RtlGetVersionFn = std::mem::transmute(proc);

        // `OSVERSIONINFOEXW`'s dw_size field is documented as the
        // struct size in bytes; the struct is well under 4 KiB, so
        // the usize→u32 narrowing is provably safe at compile time
        // — proven by the `size_of` being part of a fixed-layout
        // type, not a runtime computation.
        #[allow(clippy::cast_possible_truncation)]
        let dw_size = std::mem::size_of::<OsVersionInfoExW>() as u32;
        let mut info = OsVersionInfoExW {
            dw_size,
            dw_major: 0,
            dw_minor: 0,
            dw_build: 0,
            dw_platform_id: 0,
            sz_csd_version: [0; 128],
            w_service_pack_major: 0,
            w_service_pack_minor: 0,
            w_suite_mask: 0,
            w_product_type: 0,
            w_reserved: 0,
        };
        // STATUS_SUCCESS is 0; any other value indicates an error.
        if rtl_get_version(&raw mut info) != 0 {
            return None;
        }
        Some((info.dw_major, info.dw_minor, info.dw_build))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_path_resolves_during_tests() {
        // `cargo test` always launches the test binary as the
        // current executable, so this should never be `None` in
        // CI. Failing here means `current_exe()` itself failed,
        // which is a deeper environment problem.
        let p = program_path().expect("current_exe() resolved");
        assert!(p.is_absolute(), "program_path must be absolute");
    }

    #[test]
    fn program_dir_is_program_path_parent() {
        let path = program_path().expect("current_exe");
        let dir = program_dir().expect("program_dir");
        assert_eq!(Some(dir.as_path()), path.parent());
    }

    #[test]
    fn windows_version_returns_known_enum_value() {
        // The set of valid `winVer` values plugins observe. A
        // future Windows release would map to WV_WIN10 / WV_WIN11
        // until the table is updated; an N++ enum value the table
        // does not produce indicates a regression here.
        let v = windows_version_npp();
        // Min: WV_UNKNOWN (0). Max from our table: WV_WIN11 (17).
        assert!(
            (0..=17).contains(&v),
            "windows_version_npp returned out-of-range value {v}"
        );
    }
}
