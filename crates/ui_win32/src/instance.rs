//! One running Code++ per configuration folder.
//!
//! A second start hands its file to the Code++ that is already running
//! and exits, so "Edit with Code++" in Explorer, a double click on a
//! file Code++ is the default for, and `codepp file.txt` in a terminal
//! all open a tab in the window the user already has. Notepad++ works
//! the same way.
//!
//! It is also what keeps a session whole. Every Code++ on one
//! configuration folder reads and writes the same `session.xml` and the
//! same recovery backups, and each save prunes the backups its own
//! session does not name. Two of them running at once take turns
//! deleting each other's.
//!
//! # How a start decides
//!
//! [`start`] is the first thing `run` does, before any window exists:
//!
//! 1. It opens the lock file in the configuration folder for writing,
//!    letting others read it and nobody else write it ([`open_lock`]).
//!    Windows grants that to one process at a time, so the open is the
//!    claim: the process that has the file open is the instance, and
//!    keeps it open until its window is destroyed ([`release`]). A
//!    process that dies lets go of it with everything else it had
//!    open, so no lock is ever left behind for a start to clear away.
//!    A Code++ started with another `APPDATA`, as every scripted test
//!    run is, has a folder and so a lock of its own. So has each
//!    Windows session and each machine that shares the folder
//!    ([`lock_name`]): a window can be handed a file only from its own
//!    session, so that is as far as one instance reaches. Two Code++
//!    in two sessions on one folder still take turns at its
//!    `session.xml`, as they always did.
//! 2. The instance writes into the file which process it is and, once
//!    it has one, which window is its main window ([`mark_window`]). A
//!    process that finds the file taken reads that ([`running`]),
//!    checks that the window is that process's, and sends it one
//!    `WM_COPYDATA` holding the file's absolute path, or no path for a
//!    start without a file.
//! 3. Until the window takes the message, the start tries both steps
//!    again every [`POLL`], for at most [`WAIT`]. That covers an
//!    instance that is still creating its window, which is what every
//!    start but the first meets when Explorer starts one process a
//!    selected file, and one that is on its way out, which closes the
//!    lock once its window is destroyed and its session saved: the
//!    start that was waiting then becomes the instance itself.
//!
//! # What the instance does with it
//!
//! [`on_copydata`] runs wherever the message is dispatched, which can
//! be inside any nested message loop, so it only checks the message,
//! queues the path and posts [`WM_APP_FORWARDED`] to its own window.
//! [`on_forwarded`] opens what is queued, as a file dropped on the
//! window is opened, once the window is in a state to take it
//! ([`quiet`]); until then a timer asks again.
//!
//! # Not a security boundary, in one direction
//!
//! Any program on the same desktop, at the same integrity level or a
//! higher one, can send this message. It lets such a program open a
//! file in Code++ by its path, which it
//! could do as well by starting `codepp.exe` with the path. Windows
//! does not deliver `WM_COPYDATA` from a lower integrity level, so a
//! sandboxed process cannot send it. The receiver bounds and checks
//! what it reads all the same.
//!
//! # What a sandboxed program cannot do
//!
//! Pose as the instance, or keep Code++ from starting. Everything a
//! start trusts comes from the lock file, and a sandboxed program, at
//! low integrity, can do nothing with that file:
//!
//! - It cannot create, replace, write or delete it: Windows lets a
//!   process write nothing in a folder of a higher integrity level
//!   than its own, and the configuration folder is the user's.
//! - It cannot open it in any other way either, to read, to execute
//!   or for its attributes alone, because the file is labelled so
//!   ([`LOCK_LABEL`]). Whoever has a file open can keep it from
//!   everybody else, lock its bytes, or have Windows hold up the next
//!   open of it, so a sandboxed program is left no handle to the lock
//!   at all.
//!
//! So whoever holds the lock is a program of the user's own, and the
//! window a start sends its path to is the one that program wrote
//! down. No window is looked for by its class or by anything else a
//! program can put on a window of its own.
//!
//! A program at the user's own integrity level can hold the file open
//! and write what it likes into it. It can also read the user's files
//! and end the user's programs, so nothing is done about that one.
//! DESIGN.md §7.4 records this and the rest of what is left.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::{c_void, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::{BOOL, HSTRING, PWSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_SUCCESS,
    GENERIC_WRITE, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Security::{
    GetAce, GetKernelObjectSecurity, GetSecurityDescriptorSacl, GetSidSubAuthority,
    GetSidSubAuthorityCount, SetFileSecurityW, ACE_HEADER, ACL, LABEL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SE_SACL_PRESENT,
    SYSTEM_MANDATORY_LABEL_ACE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_ALWAYS,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::SystemInformation::{ComputerNamePhysicalNetBIOS, GetComputerNameExW};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, GetGUIThreadInfo, GetLastActivePopup, GetWindowThreadProcessId,
    IsIconic, KillTimer, PostMessageW, SendMessageTimeoutW, SetForegroundWindow, SetTimer,
    ShowWindow, GUITHREADINFO, GUI_INMENUMODE, GUI_INMOVESIZE, GUI_POPUPMENUMODE,
    GUI_SYSTEMMENUMODE, SMTO_ABORTIFHUNG, SW_RESTORE, WM_APP, WM_COPYDATA,
};

use codepp_shell::OpenFileOutcome;

/// Posted by [`on_copydata`] to the main window: a start was handed
/// over, and [`on_forwarded`] should deal with it. `WM_APP + 1` to
/// `+ 4` are the wake, the UDL editor's two and the quit wake.
pub(crate) const WM_APP_FORWARDED: u32 = WM_APP + 5;

/// The main window's timer that calls [`on_forwarded`] again while the
/// window is not in a state to open a file. Not the next small number
/// after the autosave's 1 and the workspace tree's 2: a plugin is
/// handed this window and may set a timer of its own on it, and one
/// that chose the same id would have its ticks taken for this timer's
/// and its timer killed with it. "CPFD".
pub(crate) const FORWARDED_TIMER_ID: usize = 0x4350_4644;
const _: () = assert!(
    FORWARDED_TIMER_ID > 0xFFFF,
    "a small timer id is one a plugin could pick for its own timer on the main window"
);

/// How often [`on_forwarded`] asks again, in milliseconds.
const FORWARDED_RETRY_MS: u32 = 200;

/// `COPYDATASTRUCT::dwData` of a handed-over start: "CPP1". A message
/// with another value is not ours and is left to `DefWindowProc`'s
/// answer, 0.
const MAGIC: usize = 0x4350_5031;

/// The window's answer to a start it queued.
const REPLY_TAKEN: isize = 1;
/// The window's answer to a message it will never take: one that is
/// malformed. The sender gives up at once instead of asking again
/// until [`WAIT`] is over.
const REPLY_REFUSED: isize = 2;
/// The window's answer when it cannot take a start now: it has been
/// asked to quit, or its queue is full. Also `DefWindowProc`'s answer,
/// so what a window that does not know the message says.
const REPLY_LATER: isize = 0;

/// How long a start goes on asking a running instance to take it.
const WAIT: Duration = Duration::from_secs(30);
/// How long it sleeps between two attempts.
const POLL: Duration = Duration::from_millis(50);
/// How long one attempt waits for the window's answer, in
/// milliseconds.
const SEND_TIMEOUT_MS: u32 = 2_000;

/// The longest path a message may hold, in UTF-16 units: Windows'
/// longest path.
const MAX_PATH_UNITS: usize = 32_767;
/// How many handed-over starts may wait to be opened. Past it the
/// window answers "later", so the senders wait their turn.
const MAX_QUEUED: usize = 256;
/// How many times Code++ is started again at a quit for starts that
/// were handed over and never opened ([`restart_for_unopened`]).
const MAX_RESTARTS: usize = 16;

/// The first four bytes of the lock file's record: "CPL1".
const LOCK_MAGIC: u32 = 0x4350_4C31;
/// The length of the lock file's record ([`Record`]).
const LOCK_RECORD_LEN: usize = 16;

/// `SECURITY_MANDATORY_MEDIUM_RID`: the integrity level of an ordinary
/// program. A sandboxed one runs below it.
const MEDIUM_INTEGRITY: u32 = 0x2000;
/// `SYSTEM_MANDATORY_LABEL_ACE_TYPE`: the kind of entry that carries an
/// object's integrity level.
const LABEL_ACE_TYPE: u8 = 0x11;
/// `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP`, `_NO_READ_UP` and
/// `_NO_EXECUTE_UP`: nothing below the label's level may write, read
/// or execute.
const LABEL_NOTHING_FROM_BELOW: u32 = 0x1 | 0x2 | 0x4;

/// A mandatory label: an integrity level, and what the label keeps
/// lower levels from doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Label {
    level: u32,
    policy: u32,
}

/// The label the lock file carries: medium integrity, the level of an
/// ordinary program, with nothing below it allowed to write, to read
/// or to execute. In the text Windows writes labels in,
/// `S:(ML;;NWNRNX;;;ME)`.
///
/// A file in the user's profile already refuses writes from below
/// without it. Nothing else it refuses: a sandboxed program may open
/// any of the user's files to read or to execute, and whoever has a
/// file open can keep it from everybody else. One that opened the lock
/// so while no Code++ was running would shut out every start until it
/// ended.
///
/// All three are needed, and they leave such a program no handle to
/// the file whatever it asks for. Measured with a low-integrity
/// process against each label: with write and read alone refused, it
/// opens the file to execute, sharing it with nobody, and the
/// instance's own open then fails for as long as it holds on; it also
/// opens it for its attributes alone, and takes an oplock through
/// that, which holds the next open up for ten seconds. With all three
/// refused, every open it tries fails, whatever the access asked for.
const LOCK_LABEL: Label = Label {
    level: MEDIUM_INTEGRITY,
    policy: LABEL_NOTHING_FROM_BELOW,
};

/// A [`Label`] as Windows keeps it on an object: a list of one entry,
/// which says what the label keeps from below and names the level by
/// its identifier, `S-1-16-<level>`. The three structures Windows
/// defines for it (`ACL`, `SYSTEM_MANDATORY_LABEL_ACE`, `SID`), one
/// after another, 28 bytes.
///
/// Laid out here and not had from Windows' own reader of the text
/// form, which costs a start more than two milliseconds on its first
/// call, as measured in the release build. A test holds the bytes
/// against what that reader makes of the text.
#[repr(C)]
struct LabelList {
    acl_revision: u8,
    acl_reserved: u8,
    acl_size: u16,
    ace_count: u16,
    acl_reserved_too: u16,
    ace_type: u8,
    ace_flags: u8,
    ace_size: u16,
    policy: u32,
    sid_revision: u8,
    sid_sub_authorities: u8,
    sid_authority: [u8; 6],
    level: u32,
}

const _: () = assert!(std::mem::size_of::<LabelList>() == 28);

impl Label {
    const fn list(self) -> LabelList {
        LabelList {
            // `ACL_REVISION`.
            acl_revision: 2,
            acl_reserved: 0,
            acl_size: 28,
            ace_count: 1,
            acl_reserved_too: 0,
            ace_type: LABEL_ACE_TYPE,
            ace_flags: 0,
            // The entry without the list's eight bytes before it.
            ace_size: 20,
            policy: self.policy,
            // `SID_REVISION`.
            sid_revision: 1,
            sid_sub_authorities: 1,
            // `SECURITY_MANDATORY_LABEL_AUTHORITY`, 16.
            sid_authority: [0, 0, 0, 0, 0, 16],
            level: self.level,
        }
    }
}

/// Call `with` on a security descriptor that says `label` and nothing
/// else: no owner and no access list, so that a file made with it
/// takes both as any file made in its folder does. The descriptor
/// lives for the call.
fn with_label_descriptor<R>(label: Label, with: impl FnOnce(PSECURITY_DESCRIPTOR) -> R) -> R {
    let mut list = label.list();
    let mut descriptor = SECURITY_DESCRIPTOR {
        // `SECURITY_DESCRIPTOR_REVISION`.
        Revision: 1,
        Sbz1: 0,
        Control: SE_SACL_PRESENT,
        Owner: PSID::default(),
        Group: PSID::default(),
        Sacl: (&raw mut list).cast(),
        Dacl: std::ptr::null_mut(),
    };
    with(PSECURITY_DESCRIPTOR((&raw mut descriptor).cast()))
}

thread_local! {
    /// The lock file this process holds open as the instance. UI
    /// thread only.
    static HELD: RefCell<Option<File>> = const { RefCell::new(None) };
    /// Handed-over starts not yet opened: a path, or `None` for a start
    /// without a file.
    static QUEUED: RefCell<VecDeque<Option<PathBuf>>> = const { RefCell::new(VecDeque::new()) };
    /// Whether a handed-over start has still to bring the window to the
    /// front.
    static BRING_FORWARD: Cell<bool> = const { Cell::new(false) };
}

/// What [`start`] found.
pub(crate) enum Start {
    /// This process is the instance for its configuration folder, or
    /// could not find out and starts as one.
    First,
    /// A running instance took the start. This process has nothing
    /// left to do.
    Forwarded,
    /// An instance is running and did not take the start. The text
    /// says why, for the user.
    Refused(String),
}

/// The lock file's name on a machine and in a Windows session.
///
/// Each session has its own, so the same account signed in twice, as
/// over Remote Desktop beside the console, has a Code++ in each: a
/// window in one session cannot be handed a file from the other. So
/// has each machine, for a configuration folder that several machines
/// share, as a profile kept on a file server is: a lock held on one
/// would otherwise keep Code++ from starting on every other, where the
/// window it names means nothing.
///
/// Of the machine's name only letters, digits, `-` and `_` are kept as
/// they are, so that whatever it is, the result is a file name.
fn lock_name(machine: Option<&str>, session: Option<u32>) -> String {
    let mut name = String::from("instance");
    if let Some(machine) = machine.filter(|machine| !machine.is_empty()) {
        name.push('-');
        name.extend(machine.chars().take(64).map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        }));
    }
    if let Some(session) = session {
        name.push_str(&format!("-s{session}"));
    }
    name.push_str(".lock");
    name
}

/// This machine's name, or `None` when Windows does not say.
fn machine_name() -> Option<String> {
    let mut buffer = [0u16; 64];
    let mut len = buffer.len() as u32;
    // SAFETY: the buffer is writable for the length given, which the
    // call replaces with the number of units it wrote.
    unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalNetBIOS,
            Some(PWSTR(buffer.as_mut_ptr())),
            &raw mut len,
        )
    }
    .ok()?;
    let name = String::from_utf16_lossy(buffer.get(..len as usize)?);
    (!name.is_empty()).then_some(name)
}

/// The Windows session this process runs in, or `None` when Windows
/// does not say.
fn session_id() -> Option<u32> {
    let mut session = 0u32;
    // SAFETY: writes the session's number through a live pointer.
    unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) }
        .ok()
        .map(|()| session)
}

/// What the instance writes into the lock file: which process it is
/// and which window is its main window, 0 while it has none. On disk
/// [`LOCK_MAGIC`], `pid` and `hwnd`, little-endian, [`LOCK_RECORD_LEN`]
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    pid: u32,
    hwnd: u64,
}

impl Record {
    fn encode(self) -> [u8; LOCK_RECORD_LEN] {
        let mut bytes = [0u8; LOCK_RECORD_LEN];
        bytes[..4].copy_from_slice(&LOCK_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.pid.to_le_bytes());
        bytes[8..].copy_from_slice(&self.hwnd.to_le_bytes());
        bytes
    }

    /// `None` for anything but a whole record: an empty file, which is
    /// what a lock just made is until it is written, a longer or a
    /// shorter one, or other bytes.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; LOCK_RECORD_LEN] = bytes.try_into().ok()?;
        let (magic, rest) = bytes.split_first_chunk::<4>()?;
        let (pid, hwnd) = rest.split_first_chunk::<4>()?;
        (u32::from_le_bytes(*magic) == LOCK_MAGIC).then_some(Self {
            pid: u32::from_le_bytes(*pid),
            hwnd: u64::from_le_bytes(hwnd.try_into().ok()?),
        })
    }
}

/// Why the lock file could not be opened as the instance opens it.
#[derive(Debug)]
enum LockError {
    /// Somebody has it open in a way that does not allow this open: an
    /// instance, or another program that lets nobody write the file
    /// while it has it open.
    Taken,
    /// Anything else: no folder to put it in, or a file that cannot be
    /// written.
    Failed(std::io::Error),
}

/// Open `lock` as the instance holds it, making the file and its
/// folder if they are not there: for writing, with others allowed to
/// read and nobody to write or delete. A file that was there comes
/// back holding what the last instance wrote, for the caller to write
/// over.
///
/// For writing only, though the instance could as well be allowed to
/// read its own lock: it never does, and asking for it is what costs.
/// A virus scanner looks at a file another process has written when a
/// process asks to read it, and the last instance has always written
/// this one. Measured with Windows' own scanner on, a new process
/// opening the file to read and write took 11 to 18 ms, in the test
/// rig and in a release build of Code++, three seconds after the
/// writer had gone as well as at once; opening it to write took 0.2 to
/// 0.4 ms.
///
/// A file made here carries `label` from the start. Where it cannot
/// be made with it, it is made without: one instance at a time matters
/// more than who can open the lock. Whether the file carries the label
/// is for the caller to look at ([`ensure_label`]), for a file just
/// made as for one that was there: a file system that keeps no labels
/// may take the label and drop it.
fn open_lock(lock: &Path, label: Label) -> Result<File, LockError> {
    if let Some(dir) = lock.parent() {
        std::fs::create_dir_all(dir).map_err(LockError::Failed)?;
    }
    let name = HSTRING::from(codepp_platform::long_path(lock, 0).as_os_str());
    let create = |attributes: Option<&SECURITY_ATTRIBUTES>| {
        // SAFETY: `name` is a NUL-terminated wide string and
        // `attributes`, with the descriptor it points at, outlive the
        // call. `OPEN_ALWAYS` and not `CREATE_ALWAYS`, which empties
        // the file for the caller but refuses one that has been marked
        // hidden, as a tool that copies or restores a profile may
        // mark it.
        unsafe {
            CreateFileW(
                &name,
                GENERIC_WRITE.0,
                FILE_SHARE_READ,
                attributes.map(std::ptr::from_ref),
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
    };
    let taken = |err: &windows::core::Error| err.code() == ERROR_SHARING_VIOLATION.to_hresult();
    let handle = with_label_descriptor(label, |descriptor| {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            // Not inherited: a program Code++ or a plugin starts would
            // otherwise hold the lock for as long as it runs, past this
            // instance's end.
            bInheritHandle: false.into(),
        };
        match create(Some(&attributes)) {
            Ok(handle) => Ok(handle),
            Err(err) if taken(&err) => Err(LockError::Taken),
            Err(with_label) => match create(None) {
                Ok(handle) => {
                    tracing::debug!(
                        err = ?with_label,
                        "the instance lock could not be made with its label"
                    );
                    Ok(handle)
                }
                Err(err) if taken(&err) => Err(LockError::Taken),
                Err(err) => Err(LockError::Failed(err.into())),
            },
        }
    })?;
    // SAFETY: a file handle just opened, owned by nothing else.
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

/// The last number of a security identifier, which for an integrity
/// level is the level.
///
/// # Safety
///
/// `sid` must point at a valid SID.
unsafe fn last_sub_authority(sid: PSID) -> Option<u32> {
    // SAFETY: per this function's contract; both calls only read it.
    unsafe {
        let count = *GetSidSubAuthorityCount(sid);
        (count != 0).then(|| *GetSidSubAuthority(sid, u32::from(count) - 1))
    }
}

/// The label an open file carries, or `None` when it carries none of
/// its own, as a file an ordinary program makes does not, or the label
/// cannot be read.
///
/// Asked of the file's handle directly. `GetSecurityInfo` answers the
/// same, and loads a library to do it that costs a start a millisecond
/// and a half, as measured in the release build.
fn file_label(file: &File) -> Option<Label> {
    // A descriptor that holds a label and nothing else is some fifty
    // bytes. `u64`s, so that what is read out of it is aligned.
    let mut buffer = [0u64; 32];
    let descriptor = PSECURITY_DESCRIPTOR(buffer.as_mut_ptr().cast());
    let mut needed = 0u32;
    // SAFETY: the handle is the open file's, and the buffer is
    // writable for the length given.
    unsafe {
        GetKernelObjectSecurity(
            HANDLE(file.as_raw_handle()),
            LABEL_SECURITY_INFORMATION.0,
            Some(descriptor),
            std::mem::size_of_val(&buffer) as u32,
            &raw mut needed,
        )
    }
    .ok()?;
    let mut present = BOOL::default();
    let mut defaulted = BOOL::default();
    let mut sacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: the descriptor is the one just written into the buffer,
    // which is alive; the three out-pointers are live.
    unsafe {
        GetSecurityDescriptorSacl(
            descriptor,
            &raw mut present,
            &raw mut sacl,
            &raw mut defaulted,
        )
    }
    .ok()?;
    if !present.as_bool() || sacl.is_null() {
        return None;
    }
    let mut ace: *mut c_void = std::ptr::null_mut();
    // SAFETY: `sacl` points into the buffer, at the list Windows wrote
    // there; its first entry, if it has one, starts with an
    // `ACE_HEADER`, and a label entry is a
    // `SYSTEM_MANDATORY_LABEL_ACE` whose SID begins at `SidStart`.
    unsafe {
        if (*sacl).AceCount == 0
            || GetAce(sacl, 0, &raw mut ace).is_err()
            || (*ace.cast::<ACE_HEADER>()).AceType != LABEL_ACE_TYPE
        {
            return None;
        }
        let entry = ace.cast::<SYSTEM_MANDATORY_LABEL_ACE>();
        last_sub_authority(PSID((&raw mut (*entry).SidStart).cast())).map(|level| Label {
            level,
            policy: (*entry).Mask,
        })
    }
}

/// Whether a label leaves a sandboxed program no way to open the file
/// that carries it.
fn keeps_sandboxed_out(label: Option<Label>) -> bool {
    label.is_some_and(|label| {
        label.level >= MEDIUM_INTEGRITY
            && label.policy & LABEL_NOTHING_FROM_BELOW == LABEL_NOTHING_FROM_BELOW
    })
}

/// Label the lock file if it is not: one made before it was labelled
/// so, brought back from a backup, or made where the label could not
/// be set at once. Looked at every time the lock is taken, which is
/// one question to Windows about a file that is open already.
fn ensure_label(lock: &Path, file: &File) {
    if keeps_sandboxed_out(file_label(file)) {
        return;
    }
    let name = HSTRING::from(codepp_platform::long_path(lock, 0).as_os_str());
    let set = with_label_descriptor(LOCK_LABEL, |descriptor| {
        // SAFETY: `name` is a NUL-terminated wide string and the
        // descriptor lives for the call. The call opens the file
        // itself, for no access to its bytes, so the instance's own
        // open does not stand in its way.
        unsafe { SetFileSecurityW(&name, LABEL_SECURITY_INFORMATION, descriptor) }.as_bool()
    });
    if !set || !keeps_sandboxed_out(file_label(file)) {
        tracing::warn!(
            lock = ?lock,
            "could not label the instance lock; a sandboxed program can open it, and one that \
             holds it open keeps Code++ from starting"
        );
    }
}

/// Write `record` over what the lock file holds, and cut off whatever
/// lay behind it.
fn write_record(file: &mut File, record: Record) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&record.encode())?;
    file.set_len(LOCK_RECORD_LEN as u64)
}

/// What trying to take the lock showed.
#[derive(Debug, PartialEq, Eq)]
enum Claim {
    /// This process took the lock and holds it.
    Ours,
    /// Another process holds it.
    Theirs,
    /// The lock file could not be opened at all.
    Unknown,
}

/// Try to become the instance whose lock file is `lock`.
fn claim(lock: &Path) -> Claim {
    claim_with(lock, LOCK_LABEL)
}

/// [`claim`] with the label a new lock file is made with given, so
/// that a test can ask for one that cannot be set.
fn claim_with(lock: &Path, label: Label) -> Claim {
    match open_lock(lock, label) {
        Ok(mut file) => {
            let record = Record {
                pid: std::process::id(),
                hwnd: 0,
            };
            if let Err(err) = write_record(&mut file, record) {
                // Still the instance: holding the file open is the
                // claim. A start made meanwhile finds no window to
                // ask and waits its time out.
                tracing::warn!(
                    ?err,
                    "could not write the instance lock; a second start will not find this one"
                );
            }
            ensure_label(lock, &file);
            HELD.with(|held| {
                let mut held = held.borrow_mut();
                debug_assert!(held.is_none(), "this thread already holds an instance lock");
                *held = Some(file);
            });
            Claim::Ours
        }
        Err(LockError::Taken) => Claim::Theirs,
        Err(LockError::Failed(err)) => {
            tracing::warn!(
                ?err,
                lock = ?lock,
                "could not open the instance lock; starting regardless"
            );
            Claim::Unknown
        }
    }
}

/// Let the lock go, so that the next start becomes the instance.
/// Called once the main window is destroyed and the session saved:
/// from then on this process only has to end, which can take as long
/// as a file read that does not return.
pub(crate) fn release() {
    drop(HELD.with(|held| held.borrow_mut().take()));
}

/// Start Code++ again for each start that was handed over and never
/// opened, because the quit began first. Called right after
/// [`release`], so the first of them becomes the new instance,
/// restores the session this one has just saved, and takes the rest.
///
/// The sender of each was told its start was taken and has exited, so
/// nothing else would open the file: without this, a file handed over
/// in the moment before a quit, or while a dialog kept the window
/// busy until the quit, would vanish without a word.
pub(crate) fn restart_for_unopened() {
    let unopened = take_unopened();
    if unopened.is_empty() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            tracing::warn!(
                ?err,
                count = unopened.len(),
                "handed-over files were not opened"
            );
            return;
        }
    };
    for path in unopened {
        let mut command = std::process::Command::new(&exe);
        if let Some(path) = &path {
            // After `--`, so that no file name reads as an option.
            command.arg("--").arg(path);
        }
        match command.spawn() {
            Ok(_) => tracing::info!(?path, "started Code++ again for a handed-over start"),
            Err(err) => tracing::warn!(?err, ?path, "a handed-over file was not opened"),
        }
    }
}

/// What was handed over and not opened, taken out of the queue: every
/// path once, and one start without a file if there was any and no
/// path, which is all it takes to bring Code++ back. At most
/// [`MAX_RESTARTS`] of them: each is a process started at a quit, and
/// the queue holds whatever any program on the desktop sent.
fn take_unopened() -> Vec<Option<PathBuf>> {
    let queued: Vec<Option<PathBuf>> = QUEUED.with(|queue| queue.borrow_mut().drain(..).collect());
    let mut unopened: Vec<Option<PathBuf>> = Vec::new();
    for path in queued.iter().flatten() {
        if !unopened.iter().flatten().any(|seen| seen == path) {
            unopened.push(Some(path.clone()));
        }
    }
    if unopened.is_empty() && !queued.is_empty() {
        unopened.push(None);
    }
    if unopened.len() > MAX_RESTARTS {
        tracing::warn!(
            dropped = unopened.len() - MAX_RESTARTS,
            "more handed-over files were left than Code++ is started again for"
        );
        unopened.truncate(MAX_RESTARTS);
    }
    unopened
}

/// A handed-over start that is not one: bytes [`encode`] does not
/// write.
#[derive(Debug, PartialEq, Eq)]
struct Malformed;

/// The bytes of a handed-over start: the path in UTF-16. No path is no
/// bytes.
fn encode(path: Option<&Path>) -> Vec<u8> {
    path.map(|path| {
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    })
    .unwrap_or_default()
}

/// Read a handed-over start back: its path, or `None` for a start
/// without a file. [`Malformed`] for half a UTF-16 unit, a path too
/// long for Windows or with a NUL in it, or a path that is not
/// absolute, which would be read from this process's working folder
/// and not the sender's.
fn decode(bytes: &[u8]) -> Result<Option<PathBuf>, Malformed> {
    if !bytes.len().is_multiple_of(2) || bytes.len() / 2 > MAX_PATH_UNITS {
        return Err(Malformed);
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if units.contains(&0) {
        return Err(Malformed);
    }
    let path = PathBuf::from(OsString::from_wide(&units));
    if path.is_absolute() {
        Ok(Some(path))
    } else {
        Err(Malformed)
    }
}

/// Write into the lock file that `hwnd` is this instance's main
/// window, so that a later start can find it. Called once, as soon as
/// the window exists.
pub(crate) fn mark_window(hwnd: HWND) {
    record_window(hwnd.0 as usize as u64);
}

/// Take the window out of the lock file again, so that no start is
/// sent to a window that is going. A start made from then on waits for
/// the lock itself.
pub(crate) fn unmark_window() {
    record_window(0);
}

/// Write into the lock file which window is this instance's main
/// window, 0 for none. Nothing to do for a process that holds no lock.
fn record_window(hwnd: u64) {
    HELD.with(|held| {
        let mut held = held.borrow_mut();
        let Some(file) = held.as_mut() else {
            return;
        };
        let record = Record {
            pid: std::process::id(),
            hwnd,
        };
        if let Err(err) = write_record(file, record) {
            tracing::warn!(
                ?err,
                "could not write the instance lock; a second start will not find the window"
            );
        }
    });
}

/// The lock file's record, or `None` while there is none to read: the
/// file is not there, somebody has it open who lets nobody read, or it
/// has just been made and not written yet.
fn read_record(lock: &Path) -> Option<Record> {
    // Everything allowed to everybody else, which is the one way to
    // open a file the instance has open for writing.
    let share = (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0;
    let file = OpenOptions::new()
        .read(true)
        .share_mode(share)
        .open(lock)
        .ok()?;
    let mut bytes = Vec::with_capacity(LOCK_RECORD_LEN + 1);
    file.take(LOCK_RECORD_LEN as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    Record::decode(&bytes)
}

/// The instance a start hands itself to: its main window, and the
/// process the window belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Running {
    hwnd: HWND,
    pid: u32,
}

/// The instance that holds `lock`, once it has a main window: the
/// window the lock file names, if it is a window of the process the
/// file names.
///
/// The check is against a record that has gone stale, which the file
/// holds when an instance crashed and something other than an instance
/// has the file open: the window it names is gone then, or is another
/// process's by now. Nothing about the window itself is asked, its
/// class or anything else a program can put on a window of its own:
/// that the lock's holder wrote it down is what makes it the
/// instance's.
fn running(lock: &Path) -> Option<Running> {
    let record = read_record(lock)?;
    if record.pid == 0 || record.hwnd == 0 {
        return None;
    }
    let hwnd = HWND(usize::try_from(record.hwnd).ok()? as *mut c_void);
    let mut pid = 0u32;
    // SAFETY: writes the process id through a live pointer; a handle
    // that is no window's leaves it 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
    (pid == record.pid).then_some(Running { hwnd, pid })
}

/// What one attempt to hand a start over came to.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Taken,
    Refused,
    /// Windows would not deliver the message: the instance runs at a
    /// higher integrity level, as administrator.
    Blocked,
    Later,
}

/// Send `payload` to the instance's window and wait, for a bounded
/// time, for its answer.
fn send(instance: Running, payload: &[u8]) -> Reply {
    let Ok(len) = u32::try_from(payload.len()) else {
        return Reply::Refused;
    };
    let data = COPYDATASTRUCT {
        dwData: MAGIC,
        cbData: len,
        lpData: payload.as_ptr().cast_mut().cast(),
    };
    // Let the instance bring its window to the front: this process was
    // started by the user's click and may do so, the instance may not.
    // SAFETY: takes a process id; one that is no process's is refused.
    let _ = unsafe { AllowSetForegroundWindow(instance.pid) };
    let mut answer = 0usize;
    // So that the error read below is the send's own, and not one the
    // call above left behind.
    // SAFETY: sets this thread's last-error value.
    unsafe { SetLastError(ERROR_SUCCESS) };
    // SAFETY: `data` and `payload` outlive the call, which copies the
    // bytes into the receiving process and returns once the window
    // answered or the time is up. `SMTO_ABORTIFHUNG` returns at once
    // from a window Windows already knows not to respond.
    let sent = unsafe {
        SendMessageTimeoutW(
            instance.hwnd,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&raw const data as isize),
            SMTO_ABORTIFHUNG,
            SEND_TIMEOUT_MS,
            Some(&raw mut answer),
        )
    };
    if sent.0 == 0 {
        // SAFETY: reads this thread's last-error value.
        return if unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
            Reply::Blocked
        } else {
            Reply::Later
        };
    }
    match answer.cast_signed() {
        REPLY_TAKEN => Reply::Taken,
        REPLY_REFUSED => Reply::Refused,
        _ => Reply::Later,
    }
}

/// Decide whether this process is the instance for its configuration
/// folder, and if it is not, hand `path` to the one that is.
///
/// Returns quickly for the first start: one file opened and sixteen
/// bytes written. A later start blocks until the instance answered,
/// for at most [`WAIT`].
pub(crate) fn start(path: Option<&Path>) -> Start {
    let Some(dir) = codepp_platform::config_dir() else {
        tracing::warn!("no configuration folder to keep the instance lock in; starting regardless");
        return Start::First;
    };
    let lock = dir.join(lock_name(machine_name().as_deref(), session_id()));
    start_with(&lock, path, WAIT)
}

/// [`start`] with its lock file and patience given, so that a test can
/// run it against a lock and a window of its own.
fn start_with(lock: &Path, path: Option<&Path>, wait: Duration) -> Start {
    let payload = encode(path);
    let deadline = Instant::now() + wait;
    // Whether the lock ever named a window to ask.
    let mut found = false;
    loop {
        match claim(lock) {
            Claim::Ours | Claim::Unknown => return Start::First,
            Claim::Theirs => {}
        }
        if let Some(instance) = running(lock) {
            found = true;
            match send(instance, &payload) {
                Reply::Taken => {
                    tracing::info!("handed the start to the Code++ that is already running");
                    return Start::Forwarded;
                }
                Reply::Refused => {
                    return Start::Refused(
                        "Code++ is already running and would not take this file.".to_string(),
                    );
                }
                Reply::Blocked => {
                    return Start::Refused(
                        "Code++ is already running as administrator, and Windows does not let \
                         this start hand it a file. Open the file from that window instead."
                            .to_string(),
                    );
                }
                Reply::Later => {}
            }
        }
        if Instant::now() >= deadline {
            return Start::Refused(if found {
                "Code++ is already running but is not responding, so the file was not opened. \
                 Try again once it responds, or once it has closed."
                    .to_string()
            } else {
                // No instance was ever seen: one that is stuck before
                // its window exists or after it has gone, or a program
                // that is no Code++ and has the lock file open.
                // The path is the user's own, and is shown as any
                // text that comes from outside is.
                format!(
                    "Code++ is starting or closing and did not finish, or another program has \
                     its lock file open, so the file was not opened. Try again in a moment.\n\n\
                     The lock file is {}",
                    codepp_shell::sanitize_str_for_display(&lock.display().to_string())
                )
            });
        }
        std::thread::sleep(POLL);
    }
}

/// `WM_COPYDATA` on the main window: queue a handed-over start.
///
/// Runs wherever the message is dispatched, a nested message loop
/// included, so it touches nothing but this module's own queue, and
/// takes no state borrow.
///
/// # Safety
///
/// `lparam` must be the `LPARAM` of a `WM_COPYDATA` message, which
/// Windows guarantees points at a `COPYDATASTRUCT` whose `lpData` is
/// readable for `cbData` bytes for the length of the call.
pub(crate) unsafe fn on_copydata(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let data = lparam.0 as *const COPYDATASTRUCT;
    if data.is_null() {
        return LRESULT(REPLY_LATER);
    }
    // SAFETY: per the contract above.
    let data = unsafe { &*data };
    if data.dwData != MAGIC {
        return LRESULT(REPLY_LATER);
    }
    let len = data.cbData as usize;
    if (data.lpData.is_null() && len != 0) || len > 2 * MAX_PATH_UNITS {
        return LRESULT(REPLY_REFUSED);
    }
    let bytes: &[u8] = if len == 0 {
        // A start without a file, which carries no bytes and may carry
        // no pointer to them.
        &[]
    } else {
        // SAFETY: per the contract above, `lpData` is readable for
        // `len` bytes; it is not null and `len` is bounded.
        unsafe { std::slice::from_raw_parts(data.lpData.cast::<u8>(), len) }
    };
    let accepting = !super::quit_requested();
    LRESULT(receive(hwnd, bytes, accepting))
}

/// The part of [`on_copydata`] that needs no pointer: check the bytes
/// and queue the start. `accepting` is whether the window is staying:
/// one asked to quit answers "later", and the start that was sent
/// becomes the instance itself once this one has gone.
fn receive(hwnd: HWND, bytes: &[u8], accepting: bool) -> isize {
    let Ok(path) = decode(bytes) else {
        tracing::warn!(len = bytes.len(), "refused a malformed handed-over start");
        return REPLY_REFUSED;
    };
    if !accepting {
        return REPLY_LATER;
    }
    let queued = QUEUED.with(|queue| {
        let mut queue = queue.borrow_mut();
        // A file that is waiting already is not queued again. A start
        // whose send ran out of time asks again, and the message it
        // sent first is still delivered.
        if path.is_some() && queue.contains(&path) {
            return Some(false);
        }
        if queue.len() >= MAX_QUEUED {
            return None;
        }
        queue.push_back(path);
        Some(true)
    });
    let Some(added) = queued else {
        return REPLY_LATER;
    };
    BRING_FORWARD.with(|flag| flag.set(true));
    // SAFETY: only enqueues a message for a window of this thread.
    if unsafe { PostMessageW(Some(hwnd), WM_APP_FORWARDED, WPARAM(0), LPARAM(0)) }.is_err() {
        // Nothing would open it. Take it back and let the sender ask
        // again.
        if added {
            QUEUED.with(|queue| queue.borrow_mut().pop_back());
        }
        return REPLY_LATER;
    }
    REPLY_TAKEN
}

/// Whether the main window is in a state to open a file handed to it.
///
/// Not while a modal dialog has it disabled, which covers every
/// prompt of the host's own and a plugin's dialog; not while a menu is
/// open or the window is being moved or sized, when a tab appearing
/// would change what the user is about to click; not while a close
/// holds the drain back, whose prompt is about the tab in front, which
/// an open would move; not during the startup restore or under a call
/// into a plugin, where the open would run inside code that has not
/// returned; and not while the window's state cannot be reached.
///
/// # Safety
///
/// `hwnd` must be the main window. UI thread only, with no
/// `WindowState` borrow held.
unsafe fn quiet(hwnd: HWND) -> bool {
    // SAFETY: `hwnd` is the main window, per this function's contract.
    let enabled = unsafe { IsWindowEnabled(hwnd) }.as_bool();
    enabled
        && !in_menu_or_sizing()
        && !super::DrainFreeze::active()
        && !super::STARTUP_RESTORE.with(Cell::get)
        && codepp_plugin_host::calling_plugin().is_none()
        // SAFETY: `hwnd` is the main window and no state borrow is
        // held, per this function's contract; the one taken here ends
        // with the expression.
        && unsafe { super::state_from_hwnd(hwnd) }.is_some()
}

/// Whether this thread is inside a menu's loop or a window's move or
/// size loop. Both dispatch posted messages and timers, so an open
/// could run in the middle of either.
fn in_menu_or_sizing() -> bool {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is live and says its own size; the thread is this
    // one.
    if unsafe { GetGUIThreadInfo(GetCurrentThreadId(), &raw mut info) }.is_err() {
        return false;
    }
    let busy = GUI_INMENUMODE | GUI_POPUPMENUMODE | GUI_SYSTEMMENUMODE | GUI_INMOVESIZE;
    (info.flags & busy).0 != 0
}

/// Bring Code++ to the front for a start that was handed over: out of
/// the taskbar if it is minimised, and then its main window, or the
/// dialog that has the main window disabled.
///
/// # Safety
///
/// `hwnd` must be the main window. UI thread only.
unsafe fn bring_forward(hwnd: HWND) {
    // SAFETY: all four calls take a window of this thread; a failed
    // `SetForegroundWindow` leaves the taskbar button flashing, which
    // is Windows' own fallback.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let front = if IsWindowEnabled(hwnd).as_bool() {
            hwnd
        } else {
            GetLastActivePopup(hwnd)
        };
        let _ = SetForegroundWindow(front);
    }
}

/// [`WM_APP_FORWARDED`], and the retry timer: open what was handed
/// over, or ask again shortly when the window cannot take it now.
///
/// # Safety
///
/// `hwnd` must be the main window. UI thread only, with no
/// `WindowState` borrow held.
pub(crate) unsafe fn on_forwarded(hwnd: HWND) {
    if super::quitting() {
        // The session is captured and the plugins have heard the
        // shutdown: nothing may open now. What is queued stays
        // queued, and `restart_for_unopened` starts Code++ again for
        // it once this window has gone. The senders were told "later"
        // from the moment the quit was asked for, so that is only
        // what arrived before it.
        // SAFETY: a timer of this window, set or not.
        let _ = unsafe { KillTimer(Some(hwnd), FORWARDED_TIMER_ID) };
        return;
    }
    if BRING_FORWARD.with(|flag| flag.replace(false)) {
        // SAFETY: per this function's contract.
        unsafe { bring_forward(hwnd) };
    }
    if QUEUED.with(|queue| queue.borrow().iter().all(Option::is_none)) {
        // Nothing to open: starts without a file, which only bring the
        // window forward, or a message that arrived after its files
        // were opened. No timer is kept running for that.
        QUEUED.with(|queue| queue.borrow_mut().clear());
        // SAFETY: a timer of this window, set or not.
        let _ = unsafe { KillTimer(Some(hwnd), FORWARDED_TIMER_ID) };
        return;
    }
    // SAFETY: per this function's contract.
    if !unsafe { quiet(hwnd) } {
        // SAFETY: sets a timer on a window of this thread; setting it
        // again replaces it.
        if unsafe { SetTimer(Some(hwnd), FORWARDED_TIMER_ID, FORWARDED_RETRY_MS, None) } == 0 {
            tracing::warn!("could not set the retry timer; handed-over files wait for the next");
        }
        return;
    }
    // SAFETY: a timer of this window, set or not.
    let _ = unsafe { KillTimer(Some(hwnd), FORWARDED_TIMER_ID) };
    let paths: Vec<PathBuf> = QUEUED.with(|queue| queue.borrow_mut().drain(..).flatten().collect());
    // The steps a file dropped on the window takes: open each as the
    // user's own gesture, then the tab strip, then the view when an
    // open only switched to a tab that was there already and no load
    // will arrive to bind it, then what the opens queued for plugins.
    let mut switched = false;
    for path in paths {
        let outcome = super::open_user_path(hwnd, path);
        switched |= matches!(outcome, OpenFileOutcome::SwitchedToExisting(_));
        if matches!(outcome, OpenFileOutcome::Rejected) {
            // The shell has said why to the user where it has a reason
            // to give, as for a file dropped at the tab limit.
            tracing::warn!("a handed-over file was not opened");
        }
    }
    // SAFETY: per this function's contract.
    unsafe {
        super::refresh_tab_chrome(hwnd);
        if switched {
            super::handle_tab_selchange(hwnd);
        }
        super::fire_queued_notifications(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, GetHandleInformation, LocalFree, HANDLE_FLAG_INHERIT, HLOCAL, WAIT_OBJECT_0,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, DuplicateTokenEx, GetTokenInformation, SecurityImpersonation,
        SetTokenInformation, TokenIntegrityLevel, TokenPrimary, WinLowLabelSid, SID_AND_ATTRIBUTES,
        TOKEN_ACCESS_MASK, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{
        CreateEventW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenEventW,
        OpenProcessToken, SetEvent, TerminateProcess, WaitForMultipleObjects, WaitForSingleObject,
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, EVENT_MODIFY_STATE, PROCESS_INFORMATION,
        STARTUPINFOW, SYNCHRONIZATION_SYNCHRONIZE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
        RegisterClassExW, MSG, PM_REMOVE, WINDOW_EX_STYLE, WNDCLASSEXW, WS_OVERLAPPED,
    };

    /// A folder of the test's own, and the lock file a Code++ on it
    /// would use.
    fn fresh_lock() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("a temporary folder");
        let lock = dir
            .path()
            .join(lock_name(machine_name().as_deref(), session_id()));
        (dir, lock)
    }

    /// The label of the lock this thread holds.
    fn held_label() -> Option<Label> {
        HELD.with(|held| file_label(held.borrow().as_ref().expect("a lock is held")))
    }

    /// What a labelled lock reads back as.
    const LABELLED: Option<Label> = Some(LOCK_LABEL);

    /// A label that keeps lower levels from writing and reading, and
    /// lets them execute: what the lock carried at first.
    const WEAKER: Label = Label {
        level: MEDIUM_INTEGRITY,
        policy: 0x1 | 0x2,
    };

    /// The list of labels Windows itself makes of a label's text form.
    fn list_from_text(sddl: PCWSTR) -> Vec<u8> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let mut present = BOOL::default();
        let mut defaulted = BOOL::default();
        let mut sacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `sddl` is a NUL-terminated wide string; the
        // descriptor Windows allocates is read while it is alive and
        // freed once, and the list is read for the length it states.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl,
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
            .expect("Windows reads the text");
            GetSecurityDescriptorSacl(
                descriptor,
                &raw mut present,
                &raw mut sacl,
                &raw mut defaulted,
            )
            .expect("the descriptor has a list of labels");
            assert!(present.as_bool() && !sacl.is_null());
            let bytes = std::slice::from_raw_parts(sacl.cast::<u8>(), usize::from((*sacl).AclSize))
                .to_vec();
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            bytes
        }
    }

    /// The bytes of a label as this module lays it out.
    fn list_bytes(label: Label) -> Vec<u8> {
        let list = label.list();
        // SAFETY: reads the bytes of a live value of a `repr(C)` type
        // with no padding, for its size.
        unsafe {
            std::slice::from_raw_parts(
                (&raw const list).cast::<u8>(),
                std::mem::size_of::<LabelList>(),
            )
        }
        .to_vec()
    }

    #[test]
    fn the_label_laid_out_by_hand_is_the_one_windows_writes() {
        assert_eq!(
            list_bytes(LOCK_LABEL),
            list_from_text(w!("S:(ML;;NWNRNX;;;ME)"))
        );
        assert_eq!(list_bytes(WEAKER), list_from_text(w!("S:(ML;;NWNR;;;ME)")));
        assert_eq!(
            list_bytes(Label {
                level: 0x1000,
                policy: 0x1
            }),
            list_from_text(w!("S:(ML;;NW;;;LW)")),
            "low integrity, nothing written from below"
        );
        assert_eq!(
            list_bytes(Label {
                level: 0x4000,
                policy: 0x7
            }),
            list_from_text(w!("S:(ML;;NWNRNX;;;SI)")),
            "the system's level"
        );
    }

    #[test]
    fn each_machine_and_session_has_a_lock_of_its_own() {
        assert_eq!(
            lock_name(Some("DESKTOP-1"), Some(3)),
            "instance-DESKTOP-1-s3.lock"
        );
        assert_eq!(lock_name(Some("PC_2"), Some(0)), "instance-PC_2-s0.lock");
        assert_eq!(lock_name(None, Some(3)), "instance-s3.lock");
        assert_eq!(lock_name(Some("PC"), None), "instance-PC.lock");
        assert_eq!(lock_name(None, None), "instance.lock");
        assert_eq!(lock_name(Some(""), None), "instance.lock");
        assert_eq!(
            lock_name(Some(r"a b\c/d:e.f"), Some(1)),
            "instance-a_b_c_d_e_f-s1.lock",
            "whatever the name, the result is a file name in the folder"
        );
        assert_eq!(
            lock_name(Some(&"x".repeat(200)), None).len(),
            "instance-.lock".len() + 64,
            "and not an endless one"
        );
        assert!(session_id().is_some(), "Windows names this session");
        assert!(machine_name().is_some(), "and this machine");
    }

    #[test]
    fn a_start_round_trips_with_and_without_a_path() {
        let path = Path::new(r"C:\notes\Ölfeld 😀.txt");
        assert_eq!(decode(&encode(Some(path))), Ok(Some(path.to_path_buf())));
        assert_eq!(decode(&encode(None)), Ok(None));
        assert!(encode(None).is_empty(), "no path is no bytes");
        let unc = Path::new(r"\\server\share\a.txt");
        assert_eq!(decode(&encode(Some(unc))), Ok(Some(unc.to_path_buf())));
    }

    #[test]
    fn a_path_that_is_not_valid_unicode_survives() {
        // An unpaired surrogate: a file name Windows allows.
        let name = OsString::from_wide(&[
            u16::from(b'C'),
            u16::from(b':'),
            u16::from(b'\\'),
            0xD800,
            u16::from(b'a'),
        ]);
        let path = PathBuf::from(name);
        assert_eq!(decode(&encode(Some(&path))), Ok(Some(path)));
    }

    #[test]
    fn malformed_bytes_are_refused() {
        let mut odd = encode(Some(Path::new(r"C:\a.txt")));
        odd.push(0);
        assert_eq!(decode(&odd), Err(Malformed), "half a UTF-16 unit");
        let mut nul = encode(Some(Path::new(r"C:\a")));
        nul.extend_from_slice(&[0, 0]);
        nul.extend_from_slice(&[b'b', 0]);
        assert_eq!(decode(&nul), Err(Malformed), "a NUL inside the path");
        let long = encode(Some(Path::new(&format!(
            r"C:\{}",
            "a".repeat(MAX_PATH_UNITS)
        ))));
        assert_eq!(
            decode(&long),
            Err(Malformed),
            "longer than Windows' longest path"
        );
        let longest = format!(r"C:\{}", "a".repeat(MAX_PATH_UNITS - 3));
        assert!(
            decode(&encode(Some(Path::new(&longest)))).is_ok(),
            "exactly the longest path is taken"
        );
    }

    #[test]
    fn a_relative_path_is_refused() {
        for relative in ["notes.txt", r"sub\notes.txt", r"C:notes.txt", r"\notes.txt"] {
            assert_eq!(
                decode(&encode(Some(Path::new(relative)))),
                Err(Malformed),
                "{relative} would be read from the receiver's working folder"
            );
        }
    }

    #[test]
    fn only_a_whole_record_reads_as_one() {
        let record = Record {
            pid: 0x0102_0304,
            hwnd: 0x0a0b_0c0d_0e0f_1011,
        };
        let bytes = record.encode();
        assert_eq!(bytes.len(), LOCK_RECORD_LEN);
        assert_eq!(Record::decode(&bytes), Some(record));
        assert_eq!(
            &bytes[..8],
            &[0x31, 0x4C, 0x50, 0x43, 0x04, 0x03, 0x02, 0x01],
            "the magic, then the process, little-endian"
        );
        assert_eq!(Record::decode(&[]), None, "a lock just made is empty");
        assert_eq!(Record::decode(&bytes[..15]), None, "cut short");
        let mut longer = bytes.to_vec();
        longer.push(0);
        assert_eq!(Record::decode(&longer), None, "something else's file");
        let mut other = bytes;
        other[0] ^= 1;
        assert_eq!(Record::decode(&other), None, "not the magic");
    }

    #[test]
    fn the_first_claim_holds_until_it_is_released() {
        let (_dir, lock) = fresh_lock();
        assert_eq!(claim(&lock), Claim::Ours);
        assert_eq!(claim(&lock), Claim::Theirs, "the lock is taken");
        assert_eq!(
            claim(&lock),
            Claim::Theirs,
            "and a failed claim holds nothing"
        );
        release();
        assert_eq!(claim(&lock), Claim::Ours, "free again once released");
        release();
    }

    #[test]
    fn a_claim_makes_its_folder_and_writes_who_holds_it() {
        let dir = tempfile::tempdir().expect("a temporary folder");
        // A configuration folder that does not exist yet, as on the
        // first start ever.
        let lock = dir.path().join("Code++").join("instance.lock");
        assert_eq!(read_record(&lock), None, "no file, no record");
        assert_eq!(claim(&lock), Claim::Ours);
        assert_eq!(
            read_record(&lock),
            Some(Record {
                pid: std::process::id(),
                hwnd: 0
            }),
            "this process, and no window yet"
        );
        release();
    }

    #[test]
    fn a_claim_writes_over_what_the_last_instance_left() {
        let (_dir, lock) = fresh_lock();
        let stale = Record {
            pid: 4,
            hwnd: 0x1234,
        };
        // Longer than a record, as nothing Code++ writes is: the claim
        // must not leave the tail behind either.
        let mut old = stale.encode().to_vec();
        old.extend_from_slice(b"left over");
        std::fs::write(&lock, &old).expect("the old lock file");
        assert_eq!(claim(&lock), Claim::Ours);
        assert_eq!(
            std::fs::metadata(&lock).expect("the lock file").len(),
            LOCK_RECORD_LEN as u64
        );
        assert_eq!(
            read_record(&lock),
            Some(Record {
                pid: std::process::id(),
                hwnd: 0
            })
        );
        release();
    }

    #[test]
    fn a_failed_claim_leaves_the_instances_record_alone() {
        let (_dir, lock) = fresh_lock();
        assert_eq!(claim(&lock), Claim::Ours);
        record_window(0x1234);
        let before = read_record(&lock);
        assert_eq!(
            before,
            Some(Record {
                pid: std::process::id(),
                hwnd: 0x1234
            })
        );
        assert_eq!(claim(&lock), Claim::Theirs);
        assert_eq!(read_record(&lock), before);
        release();
    }

    #[test]
    fn a_lock_file_marked_hidden_is_still_taken() {
        let (_dir, lock) = fresh_lock();
        std::fs::write(&lock, b"").expect("the lock file");
        // SAFETY: sets an attribute of a file this test made.
        unsafe { SetFileAttributesW(&HSTRING::from(lock.as_os_str()), FILE_ATTRIBUTE_HIDDEN) }
            .expect("the file is hidden");
        assert_eq!(
            claim(&lock),
            Claim::Ours,
            "a tool that marks the profile's files hidden turns the lock off"
        );
        release();
    }

    #[test]
    fn the_lock_is_labelled_against_sandboxed_programs() {
        assert!(!keeps_sandboxed_out(None), "no label of its own");
        for policy in 0..LABEL_NOTHING_FROM_BELOW {
            let level = MEDIUM_INTEGRITY;
            assert!(
                !keeps_sandboxed_out(Some(Label { level, policy })),
                "a label of {policy:#x} leaves a way to open the file from below"
            );
        }
        let policy = LABEL_NOTHING_FROM_BELOW;
        assert!(
            !keeps_sandboxed_out(Some(Label {
                level: 0x1000,
                policy
            })),
            "a low label keeps nothing at low integrity out"
        );
        assert!(keeps_sandboxed_out(LABELLED));
        assert!(
            keeps_sandboxed_out(Some(Label {
                level: 0x3000,
                policy
            })),
            "high"
        );

        // A lock is labelled as it is made, not a moment after: there
        // is no moment at which a sandboxed program could open it.
        let (_dir, lock) = fresh_lock();
        let made = open_lock(&lock, LOCK_LABEL).expect("the lock is made");
        assert_eq!(file_label(&made), LABELLED);
        drop(made);
        let (_dir, lock) = fresh_lock();
        assert_eq!(claim(&lock), Claim::Ours);
        assert_eq!(held_label(), LABELLED);
        release();
        // And keeps its label when the next instance takes it.
        assert_eq!(claim(&lock), Claim::Ours);
        assert_eq!(held_label(), LABELLED);
        release();

        // One that was there without a label, as after a restore from a
        // backup, is labelled when it is taken.
        let (_dir, lock) = fresh_lock();
        std::fs::write(&lock, b"").expect("an unlabelled lock file");
        assert_eq!(
            file_label(&File::open(&lock).expect("the file opens")),
            None,
            "a file an ordinary program makes carries no label"
        );
        assert_eq!(claim(&lock), Claim::Ours);
        assert_eq!(held_label(), LABELLED);
        release();

        // And so is one that carries a weaker label.
        let (_dir, lock) = fresh_lock();
        let weaker = open_lock(&lock, WEAKER).expect("the lock is made");
        assert_eq!(file_label(&weaker), Some(WEAKER));
        drop(weaker);
        assert_eq!(claim(&lock), Claim::Ours);
        assert_eq!(held_label(), LABELLED);
        release();
    }

    #[test]
    fn a_lock_that_cannot_be_made_with_its_label_is_made_without() {
        // The system's own integrity level, which no program may label
        // a file with: what a file system that keeps no labels comes
        // to.
        let system = Label {
            level: 0x4000,
            policy: LABEL_NOTHING_FROM_BELOW,
        };
        let (_dir, lock) = fresh_lock();
        assert_eq!(
            claim_with(&lock, system),
            Claim::Ours,
            "one instance at a time matters more than the label"
        );
        // Made without, then labelled by `ensure_label`, which can
        // here. Were the file made with the label asked for, it would
        // read back as the system's level.
        assert_eq!(held_label(), LABELLED);
        release();
    }

    #[test]
    fn the_instance_asks_for_no_way_to_read_its_lock() {
        let (_dir, lock) = fresh_lock();
        assert_eq!(claim(&lock), Claim::Ours);
        let mut byte = [0u8; 1];
        let read = HELD.with(|held| {
            let mut held = held.borrow_mut();
            let file = held.as_mut().expect("a lock is held");
            file.seek(SeekFrom::Start(0))
                .expect("the start of the file");
            file.read(&mut byte)
        });
        assert!(
            read.is_err(),
            "the lock is open for reading too, which has a virus scanner look at it at every \
             start: read {read:?}"
        );
        // Everybody else reads it all the same, which is how a second
        // start finds the window.
        assert!(read_record(&lock).is_some());
        // And the instance still writes it, and reads its label.
        record_window(7);
        assert_eq!(
            read_record(&lock),
            Some(Record {
                pid: std::process::id(),
                hwnd: 7
            })
        );
        assert_eq!(held_label(), LABELLED);
        release();
    }

    #[test]
    fn the_lock_is_not_handed_to_programs_code_plus_plus_starts() {
        let (_dir, lock) = fresh_lock();
        assert_eq!(claim(&lock), Claim::Ours);
        let mut flags = 0u32;
        HELD.with(|held| {
            let held = held.borrow();
            let file = held.as_ref().expect("a lock is held");
            // SAFETY: reads the flags of a handle this thread owns.
            unsafe { GetHandleInformation(HANDLE(file.as_raw_handle()), &raw mut flags) }
                .expect("the handle's flags");
        });
        assert_eq!(
            flags & HANDLE_FLAG_INHERIT.0,
            0,
            "a child process would hold the lock past this instance's end"
        );
        release();
    }

    #[test]
    fn a_program_that_only_reads_the_lock_keeps_no_start_out() {
        let (_dir, lock) = fresh_lock();
        std::fs::write(&lock, b"").expect("the lock file");
        // Open as a virus scanner or a backup opens a file: to read,
        // with everything allowed to everybody else.
        let reader = File::open(&lock).expect("a reader");
        assert_eq!(claim(&lock), Claim::Ours);
        drop(reader);
        release();
    }

    #[test]
    fn a_program_that_lets_nobody_write_the_lock_is_waited_for() {
        let (_dir, lock) = fresh_lock();
        // What an instance that crashed left: a window that is gone.
        let stale = Record {
            pid: std::process::id(),
            hwnd: 0x7fff_fff0,
        };
        std::fs::write(&lock, stale.encode()).expect("the lock file");
        let squatter = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&lock)
            .expect("a reader that lets nobody write");
        assert_eq!(claim(&lock), Claim::Theirs, "nobody else may write it now");
        assert_eq!(
            running(&lock),
            None,
            "the record is the dead instance's and names no window"
        );
        let Start::Refused(why) = start_with(&lock, None, Duration::from_millis(150)) else {
            panic!("it starts beside what may be an instance");
        };
        assert!(
            why.contains(&lock.display().to_string()) && why.contains("another program"),
            "the user is not told which file to look at: {why}"
        );
        assert_eq!(
            std::fs::read(&lock).expect("the lock file"),
            stale.encode(),
            "a start that is not the instance writes nothing"
        );
        drop(squatter);
        assert_eq!(claim(&lock), Claim::Ours, "free once the reader has gone");
        release();
    }

    #[test]
    fn a_record_that_names_no_window_names_no_instance() {
        let (_dir, lock) = fresh_lock();
        let own = std::process::id();
        // No window of anybody's has this handle.
        let no_window = 0x7fff_fff0;
        let write = |record: Record| std::fs::write(&lock, record.encode()).expect("the lock");
        write(Record { pid: own, hwnd: 0 });
        assert_eq!(running(&lock), None, "an instance with no window yet");
        write(Record {
            pid: own,
            hwnd: no_window,
        });
        assert_eq!(running(&lock), None, "a window that is gone");
        write(Record {
            pid: 0,
            hwnd: no_window,
        });
        assert_eq!(
            running(&lock),
            None,
            "no process, which is also what a handle that is no window's belongs to"
        );
        write(Record {
            pid: own,
            hwnd: u64::MAX,
        });
        assert_eq!(running(&lock), None);
        // A whole record with something after it is not one Code++
        // wrote.
        let mut longer = Record { pid: own, hwnd: 0 }.encode().to_vec();
        longer.push(b'!');
        std::fs::write(&lock, &longer).expect("the lock");
        assert_eq!(read_record(&lock), None);
        std::fs::write(&lock, b"").expect("the lock");
        assert_eq!(read_record(&lock), None, "made and not written yet");
    }

    #[test]
    fn a_start_with_no_instance_is_the_first() {
        let (_dir, lock) = fresh_lock();
        let started = start_with(&lock, None, Duration::ZERO);
        assert!(matches!(started, Start::First));
        assert!(
            HELD.with(|held| held.borrow().is_some()),
            "and holds the lock"
        );
        release();
    }

    #[test]
    fn a_start_goes_ahead_where_no_lock_can_be_kept() {
        let dir = tempfile::tempdir().expect("a temporary folder");
        // A file where the configuration folder should be.
        let not_a_folder = dir.path().join("Code++");
        std::fs::write(&not_a_folder, b"").expect("the file in the way");
        let lock = not_a_folder.join("instance.lock");
        assert_eq!(claim(&lock), Claim::Unknown);
        let started = start_with(&lock, None, Duration::ZERO);
        assert!(
            matches!(started, Start::First),
            "Code++ starts all the same, as it did before it kept a lock"
        );
        assert!(HELD.with(|held| held.borrow().is_none()), "holding nothing");
        // Nothing to write a window into, and nothing goes wrong.
        record_window(1);
    }

    #[test]
    fn a_start_gives_up_when_the_instance_has_no_window() {
        // The lock is held by this thread and names no window: what a
        // start meets while the instance is hung before its window
        // exists.
        let (_dir, lock) = fresh_lock();
        assert_eq!(claim(&lock), Claim::Ours);
        let began = Instant::now();
        let theirs = lock.clone();
        let started =
            std::thread::spawn(move || start_with(&theirs, None, Duration::from_millis(200)))
                .join()
                .expect("the start returns");
        assert!(
            matches!(started, Start::Refused(_)),
            "it does not start beside the instance"
        );
        assert!(
            began.elapsed() >= Duration::from_millis(200),
            "it waited its time out"
        );
        release();
    }

    #[test]
    fn a_start_becomes_the_instance_once_the_old_one_lets_go() {
        let (_dir, lock) = fresh_lock();
        assert_eq!(claim(&lock), Claim::Ours);
        let theirs = lock.clone();
        let waiting = std::thread::spawn(move || {
            let started = start_with(&theirs, None, Duration::from_secs(20));
            // The lock belongs to that thread now; give it back so the
            // test leaves nothing held.
            release();
            started
        });
        std::thread::sleep(Duration::from_millis(150));
        release();
        let started = waiting.join().expect("the start returns");
        assert!(matches!(started, Start::First));
    }

    #[test]
    fn receive_checks_the_bytes_and_whether_the_window_is_staying() {
        let good = encode(Some(Path::new(r"C:\a.txt")));
        // A null window: the post goes to this thread's own queue.
        let hwnd = HWND::default();
        assert_eq!(receive(hwnd, &[1, 2, 3], true), REPLY_REFUSED, "malformed");
        assert_eq!(
            receive(hwnd, &encode(Some(Path::new("a.txt"))), true),
            REPLY_REFUSED,
            "a relative path"
        );
        assert_eq!(receive(hwnd, &good, false), REPLY_LATER, "asked to quit");
        assert!(
            QUEUED.with(|q| q.borrow().is_empty()),
            "nothing refused is queued"
        );
        assert!(
            !BRING_FORWARD.with(Cell::get),
            "nor brings the window forward"
        );

        assert_eq!(receive(hwnd, &good, true), REPLY_TAKEN);
        assert_eq!(receive(hwnd, &encode(None), true), REPLY_TAKEN);
        let queued: Vec<_> = QUEUED.with(|q| q.borrow_mut().drain(..).collect());
        assert_eq!(queued, vec![Some(PathBuf::from(r"C:\a.txt")), None]);
        assert!(
            BRING_FORWARD.with(|f| f.replace(false)),
            "a taken start comes to the front"
        );
    }

    #[test]
    fn a_file_that_is_waiting_is_not_queued_twice() {
        let hwnd = HWND::default();
        let a = encode(Some(Path::new(r"C:\a.txt")));
        let b = encode(Some(Path::new(r"C:\b.txt")));
        // A start whose send ran out of time sends again, and both
        // messages arrive.
        assert_eq!(receive(hwnd, &a, true), REPLY_TAKEN);
        assert_eq!(receive(hwnd, &b, true), REPLY_TAKEN);
        assert_eq!(receive(hwnd, &a, true), REPLY_TAKEN, "taken: it will open");
        let queued: Vec<_> = QUEUED.with(|q| q.borrow_mut().drain(..).collect());
        assert_eq!(
            queued,
            vec![
                Some(PathBuf::from(r"C:\a.txt")),
                Some(PathBuf::from(r"C:\b.txt"))
            ]
        );
        BRING_FORWARD.with(|f| f.set(false));
    }

    #[test]
    fn what_was_never_opened_is_taken_once_each() {
        let a = PathBuf::from(r"C:\a.txt");
        let b = PathBuf::from(r"C:\b.txt");
        assert!(
            take_unopened().is_empty(),
            "nothing queued, nothing to restart for"
        );
        QUEUED.with(|q| {
            q.borrow_mut()
                .extend([Some(a.clone()), None, Some(b.clone()), Some(a.clone())]);
        });
        assert_eq!(
            take_unopened(),
            vec![Some(a), Some(b)],
            "each file once; a start for a file brings Code++ back by itself"
        );
        assert!(
            QUEUED.with(|q| q.borrow().is_empty()),
            "the queue is emptied"
        );
        QUEUED.with(|q| q.borrow_mut().extend([None, None]));
        assert_eq!(
            take_unopened(),
            vec![None],
            "starts without a file come to one"
        );
        QUEUED.with(|q| {
            q.borrow_mut()
                .extend((0..MAX_RESTARTS + 5).map(|n| Some(PathBuf::from(format!(r"C:\{n}.txt")))));
        });
        let capped = take_unopened();
        assert_eq!(capped.len(), MAX_RESTARTS, "a quit starts a bounded number");
        assert_eq!(
            capped[0],
            Some(PathBuf::from(r"C:\0.txt")),
            "the first ones"
        );
        assert!(QUEUED.with(|q| q.borrow().is_empty()));
    }

    #[test]
    fn a_message_is_checked_before_its_bytes_are_read() {
        let hwnd = HWND::default();
        let ask = |magic: usize, data: *const u8, len: u32| {
            let message = COPYDATASTRUCT {
                dwData: magic,
                cbData: len,
                lpData: data.cast_mut().cast(),
            };
            // SAFETY: the structure and what it points at outlive the
            // call; the lengths given are those of the buffers, or the
            // call returns before reading.
            unsafe { on_copydata(hwnd, LPARAM(&raw const message as isize)) }.0
        };
        let good = encode(Some(Path::new(r"C:\a.txt")));
        let len = u32::try_from(good.len()).unwrap();
        // SAFETY: a null `LPARAM` is refused before anything is read.
        assert_eq!(unsafe { on_copydata(hwnd, LPARAM(0)) }.0, REPLY_LATER);
        assert_eq!(ask(MAGIC + 1, good.as_ptr(), len), REPLY_LATER, "not ours");
        assert_eq!(ask(MAGIC, std::ptr::null(), len), REPLY_REFUSED, "no bytes");
        // A length past the bound is refused without the bytes being
        // read: there are none behind this pointer to read.
        let too_long = u32::try_from(2 * MAX_PATH_UNITS + 2).unwrap();
        assert_eq!(ask(MAGIC, good.as_ptr(), too_long), REPLY_REFUSED);
        assert!(QUEUED.with(|q| q.borrow().is_empty()));
        assert_eq!(ask(MAGIC, good.as_ptr(), len), REPLY_TAKEN);
        // A start without a file is no bytes, with or without a pointer
        // to them.
        assert_eq!(ask(MAGIC, std::ptr::null(), 0), REPLY_TAKEN);
        assert_eq!(ask(MAGIC, good.as_ptr(), 0), REPLY_TAKEN);
        let queued: Vec<_> = QUEUED.with(|q| q.borrow_mut().drain(..).collect());
        assert_eq!(queued, vec![Some(PathBuf::from(r"C:\a.txt")), None, None]);
        BRING_FORWARD.with(|f| f.set(false));
    }

    #[test]
    fn a_full_queue_answers_later() {
        let hwnd = HWND::default();
        for n in 0..MAX_QUEUED {
            let start = if n % 2 == 0 {
                encode(None)
            } else {
                encode(Some(Path::new(&format!(r"C:\{n}.txt"))))
            };
            assert_eq!(receive(hwnd, &start, true), REPLY_TAKEN);
        }
        assert_eq!(receive(hwnd, &encode(None), true), REPLY_LATER);
        assert_eq!(
            receive(hwnd, &encode(Some(Path::new(r"C:\more.txt"))), true),
            REPLY_LATER
        );
        assert_eq!(QUEUED.with(|q| q.borrow().len()), MAX_QUEUED);
        QUEUED.with(|q| q.borrow_mut().clear());
        BRING_FORWARD.with(|f| f.set(false));
    }

    /// The integrity level this process runs at, or `None` when it
    /// cannot be read.
    fn own_integrity() -> Option<u32> {
        let mut token = HANDLE::default();
        // SAFETY: the pseudo-handle of this process needs no closing;
        // the token's handle is written through a live pointer.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.ok()?;
        // A label and the SID it points at, which the call writes
        // behind it. `u64`s, so that the label is aligned; a SID is at
        // most 68 bytes.
        let mut buffer = [0u64; 16];
        let mut written = 0u32;
        // SAFETY: the buffer is writable for the length given.
        let read = unsafe {
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                Some(buffer.as_mut_ptr().cast()),
                std::mem::size_of_val(&buffer) as u32,
                &raw mut written,
            )
        };
        // SAFETY: on success the buffer holds a `TOKEN_MANDATORY_LABEL`
        // whose SID lies inside the buffer, which is still alive.
        let level = read.ok().and_then(|()| unsafe {
            let label = &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
            last_sub_authority(label.Label.Sid)
        });
        // SAFETY: the handle opened above, closed once.
        let _ = unsafe { CloseHandle(token) };
        level
    }

    /// The environment variable that turns [`sandboxed_child`] from a
    /// test that does nothing into the sandboxed program. Its value is
    /// the folder to try.
    const SANDBOX_ENV: &str = "CODEPP_INSTANCE_TEST_SANDBOX";
    /// The variable that names the two events the child and the test
    /// talk through: its value with `-ready` and `-done` after it.
    const SANDBOX_EVENTS_ENV: &str = "CODEPP_INSTANCE_TEST_SANDBOX_EVENTS";
    /// What [`sandboxed_child`] calls the lock file in that folder.
    const SANDBOX_LOCK: &str = "instance.lock";
    /// A file beside it that carries no label.
    const SANDBOX_CONTROL: &str = "control.txt";

    /// Set in the child's exit status so that it cannot be mistaken
    /// for the test harness's own 0 or 101.
    const CHILD_RAN: u32 = 0x100;
    const CHILD_IS_SANDBOXED: u32 = 0x01;
    const CHILD_READ_THE_CONTROL: u32 = 0x02;
    const CHILD_OPENED_THE_LOCK: u32 = 0x04;
    const CHILD_MADE_A_FILE: u32 = 0x08;
    const CHILD_MOVED_THE_LOCK: u32 = 0x10;
    const CHILD_DELETED_THE_LOCK: u32 = 0x20;

    /// Every way to ask for a file: each right a file has, each right
    /// every object has, "whatever I may have", and the four generic
    /// ones. A handle with any of them is a handle to hold on to.
    const EVERY_ACCESS: [u32; 18] = [
        0x1,         // FILE_READ_DATA
        0x2,         // FILE_WRITE_DATA
        0x4,         // FILE_APPEND_DATA
        0x8,         // FILE_READ_EA
        0x10,        // FILE_WRITE_EA
        0x20,        // FILE_EXECUTE
        0x80,        // FILE_READ_ATTRIBUTES
        0x100,       // FILE_WRITE_ATTRIBUTES
        0x1_0000,    // DELETE
        0x2_0000,    // READ_CONTROL
        0x4_0000,    // WRITE_DAC
        0x8_0000,    // WRITE_OWNER
        0x10_0000,   // SYNCHRONIZE
        0x200_0000,  // MAXIMUM_ALLOWED
        0x1000_0000, // GENERIC_ALL
        0x2000_0000, // GENERIC_EXECUTE
        0x4000_0000, // GENERIC_WRITE
        0x8000_0000, // GENERIC_READ
    ];

    /// One of the two events the sandboxed child and its test talk
    /// through, opened by name.
    fn sandbox_event(prefix: &str, which: &str) -> windows::core::Result<HANDLE> {
        let name = HSTRING::from(format!("{prefix}-{which}"));
        // SAFETY: opens a named event; the name outlives the call.
        unsafe {
            OpenEventW(
                EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
                false,
                &name,
            )
        }
    }

    /// Not a test of its own: the body of the low-integrity process
    /// [`a_sandboxed_program_can_do_nothing_with_the_lock`] starts,
    /// which is this test binary run for this one test. It opens the
    /// lock file in every way it can and holds on to what it gets,
    /// tells the test so, and waits for it. Then it tries to get the
    /// file out of the way, and says in its exit status what worked.
    /// Run as an ordinary test, without the variable, it returns at
    /// once.
    #[test]
    fn sandboxed_child() {
        let Some(dir) = std::env::var_os(SANDBOX_ENV) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let events = std::env::var(SANDBOX_EVENTS_ENV).expect("the events' name");
        let ready = sandbox_event(&events, "ready").expect("the ready event");
        let done = sandbox_event(&events, "done").expect("the done event");
        let lock = dir.join(SANDBOX_LOCK);
        let mut did = CHILD_RAN;
        if own_integrity().is_some_and(|level| level < MEDIUM_INTEGRITY) {
            did |= CHILD_IS_SANDBOXED;
        }
        if std::fs::read(dir.join(SANDBOX_CONTROL)).is_ok() {
            did |= CHILD_READ_THE_CONTROL;
        }
        // Sharing with nobody first, which is the open that shuts a
        // start out, then with everybody, which any handle is enough
        // for to take an oplock or lock the file's bytes.
        let mut held = Vec::new();
        for access in EVERY_ACCESS {
            for share in [
                0,
                (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0,
            ] {
                if let Ok(file) = OpenOptions::new()
                    .access_mode(access)
                    .share_mode(share)
                    .open(&lock)
                {
                    eprintln!("opened the lock with access {access:#x}, sharing {share:#x}");
                    did |= CHILD_OPENED_THE_LOCK;
                    held.push(file);
                }
            }
        }
        // SAFETY: both are events this process opened.
        unsafe {
            SetEvent(ready).expect("the test is told");
            WaitForSingleObject(done, 60_000);
        }
        drop(held);
        if File::create(dir.join("planted.lock")).is_ok() {
            did |= CHILD_MADE_A_FILE;
        }
        if std::fs::rename(&lock, dir.join("moved.lock")).is_ok() {
            did |= CHILD_MOVED_THE_LOCK;
        }
        if std::fs::remove_file(&lock).is_ok() {
            did |= CHILD_DELETED_THE_LOCK;
        }
        std::process::exit(did.cast_signed());
    }

    /// Start this test binary again as [`sandboxed_child`], at low
    /// integrity, as a sandboxed process runs. Lowering the level of a
    /// copy of one's own token and starting a process with it takes no
    /// privilege.
    fn spawn_sandboxed_child(dir: &Path, events: &str) -> HANDLE {
        /// `SE_GROUP_INTEGRITY`: the SID is an integrity level.
        const SE_GROUP_INTEGRITY: u32 = 0x20;
        let exe = std::env::current_exe().expect("the test binary's path");
        let mut command: Vec<u16> = format!(
            "\"{}\" instance::tests::sandboxed_child --exact --nocapture",
            exe.display()
        )
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
        // This process's environment and the two variables more.
        let mut environment: Vec<u16> = Vec::new();
        for (name, value) in std::env::vars_os() {
            environment.extend(name.encode_wide());
            environment.push(u16::from(b'='));
            environment.extend(value.encode_wide());
            environment.push(0);
        }
        environment.extend(SANDBOX_ENV.encode_utf16());
        environment.push(u16::from(b'='));
        environment.extend(dir.as_os_str().encode_wide());
        environment.push(0);
        environment.extend(format!("{SANDBOX_EVENTS_ENV}={events}").encode_utf16());
        environment.extend([0, 0]);
        // SAFETY: every pointer passed is to a live local, sized as the
        // call is told; the handles opened are closed below.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_ADJUST_DEFAULT | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
                &raw mut token,
            )
            .expect("this process's token");
            let mut low = HANDLE::default();
            DuplicateTokenEx(
                token,
                TOKEN_ACCESS_MASK(0),
                None,
                SecurityImpersonation,
                TokenPrimary,
                &raw mut low,
            )
            .expect("a copy of the token");
            let mut sid = [0u64; 16];
            let mut sid_len = std::mem::size_of_val(&sid) as u32;
            CreateWellKnownSid(
                WinLowLabelSid,
                None,
                Some(PSID(sid.as_mut_ptr().cast())),
                &raw mut sid_len,
            )
            .expect("the low integrity level's SID");
            let label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES {
                    Sid: PSID(sid.as_mut_ptr().cast()),
                    Attributes: SE_GROUP_INTEGRITY,
                },
            };
            SetTokenInformation(
                low,
                TokenIntegrityLevel,
                (&raw const label).cast(),
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + sid_len,
            )
            .expect("the copy lowered to low integrity");
            let startup = STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                ..Default::default()
            };
            let mut started = PROCESS_INFORMATION::default();
            CreateProcessAsUserW(
                Some(low),
                PCWSTR::null(),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                Some(environment.as_ptr().cast()),
                PCWSTR::null(),
                &raw const startup,
                &raw mut started,
            )
            .expect("the low-integrity process starts");
            let _ = CloseHandle(started.hThread);
            let _ = CloseHandle(low);
            let _ = CloseHandle(token);
            started.hProcess
        }
    }

    /// A sandboxed program gets no handle to the lock file, whatever it
    /// asks for, and cannot put another file in its place, while no
    /// Code++ is running: the moment at which holding the file would
    /// shut every start out. And whatever it does hold, a start still
    /// takes the lock.
    #[test]
    fn a_sandboxed_program_can_do_nothing_with_the_lock() {
        struct Kill(HANDLE);
        impl Drop for Kill {
            fn drop(&mut self) {
                // SAFETY: the process handle this test was given.
                unsafe {
                    let _ = TerminateProcess(self.0, 0);
                    let _ = CloseHandle(self.0);
                }
            }
        }
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = tempfile::tempdir().expect("a temporary folder");
        let lock = dir.path().join(SANDBOX_LOCK);
        std::fs::write(dir.path().join(SANDBOX_CONTROL), b"control").expect("the control file");
        // An instance comes and goes, and leaves its lock file behind.
        assert_eq!(claim(&lock), Claim::Ours);
        assert_eq!(held_label(), LABELLED);
        release();
        assert!(
            File::open(&lock).is_ok(),
            "an ordinary program reads the lock, so the child is given the right path"
        );

        // Two events a low-integrity process may set and wait on, which
        // takes a label that says so.
        let events = format!(
            "Local\\codepp-instance-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let low = Label {
            level: 0x1000,
            policy: 0x1,
        };
        let event = |which: &str| {
            let name = HSTRING::from(format!("{events}-{which}"));
            with_label_descriptor(low, |descriptor| {
                let attributes = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor.0,
                    bInheritHandle: false.into(),
                };
                // SAFETY: `attributes` and the descriptor it points at
                // outlive the call, as does the name.
                unsafe { CreateEventW(Some(&raw const attributes), true, false, &name) }
                    .expect("an event")
            })
        };
        let ready = event("ready");
        let done = event("done");

        // An account that may not start a process with a lowered copy
        // of its own token cannot run this. Under CI that is a failure
        // to look into, not a skip nobody sees.
        let started = std::panic::catch_unwind(|| spawn_sandboxed_child(dir.path(), &events));
        let Ok(child) = started else {
            assert!(
                std::env::var_os("CI").is_none(),
                "no low-integrity process could be started, so the sandbox test did not run"
            );
            eprintln!("skipped: this account cannot start a low-integrity process");
            return;
        };
        let child = Kill(child);
        // SAFETY: waits on two handles this test owns.
        let woke = unsafe { WaitForMultipleObjects(&[ready, child.0], false, 60_000) };
        assert_eq!(
            woke, WAIT_OBJECT_0,
            "the sandboxed process ended, or never got as far as holding what it could open"
        );
        // It now holds every handle to the lock it could get.
        assert_eq!(
            claim(&lock),
            Claim::Ours,
            "a sandboxed program holds the lock file in a way that shuts a start out"
        );
        assert_eq!(
            read_record(&lock),
            Some(Record {
                pid: std::process::id(),
                hwnd: 0
            }),
            "and in a way that keeps the instance from writing into it"
        );
        release();

        let mut status = 0u32;
        // SAFETY: sets an event, waits on a process and reads its exit
        // status, all through handles this test owns.
        unsafe {
            SetEvent(done).expect("the child is let go");
            assert_eq!(
                WaitForSingleObject(child.0, 60_000),
                WAIT_OBJECT_0,
                "the sandboxed process never ended"
            );
            GetExitCodeProcess(child.0, &raw mut status).expect("its exit status");
            let _ = CloseHandle(ready);
            let _ = CloseHandle(done);
        }
        assert_eq!(
            status & CHILD_RAN,
            CHILD_RAN,
            "the child did not run its test: exit status {status:#x}"
        );
        assert_ne!(
            status & CHILD_IS_SANDBOXED,
            0,
            "the child did not run below an ordinary program"
        );
        assert_ne!(
            status & CHILD_READ_THE_CONTROL,
            0,
            "the child could not read even an unlabelled file there, so this shows nothing \
             about the label"
        );
        for (bit, what) in [
            (CHILD_OPENED_THE_LOCK, "got a handle to the lock"),
            (CHILD_MADE_A_FILE, "made a file in the folder"),
            (CHILD_MOVED_THE_LOCK, "moved the lock away"),
            (CHILD_DELETED_THE_LOCK, "deleted the lock"),
        ] {
            assert_eq!(status & bit, 0, "a sandboxed program {what}");
        }
        // And the next start is the instance.
        let started = start_with(&lock, None, Duration::ZERO);
        assert!(matches!(started, Start::First));
        release();
    }

    /// The window procedure of the test's stand-in for the main
    /// window: `WM_COPYDATA` goes to the real receiver.
    unsafe extern "system" fn test_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_COPYDATA {
            // SAFETY: Windows hands a `WM_COPYDATA` its structure.
            return unsafe { on_copydata(hwnd, lparam) };
        }
        // SAFETY: the default procedure, with the message as it came.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// Run a start on another thread and pump this one's messages until
    /// it has its answer: a sent message is delivered only while the
    /// receiving thread looks for messages.
    fn start_while_pumping(lock: &Path, path: Option<&Path>, wait: Duration) -> Start {
        let lock = lock.to_path_buf();
        let path = path.map(Path::to_path_buf);
        let sender = std::thread::spawn(move || start_with(&lock, path.as_deref(), wait));
        let began = Instant::now();
        while !sender.is_finished() {
            assert!(
                began.elapsed() < Duration::from_mins(1),
                "the start never returned"
            );
            let mut msg = MSG::default();
            // SAFETY: an ordinary message pump on this thread.
            unsafe {
                while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&raw const msg);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.join().expect("the start returns")
    }

    /// The whole hand-over against a real window: a "second start" on
    /// another thread finds the window through the lock file, sends
    /// the message through Windows, and the receiver queues the path.
    #[test]
    fn a_second_start_reaches_the_window_the_lock_names() {
        let class = w!("CodeppInstanceTestWindow");
        // SAFETY: registers a class and creates a hidden window on
        // this thread, which also destroys it.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(None).expect("a module handle");
            let wc = WNDCLASSEXW {
                cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap(),
                lpfnWndProc: Some(test_wnd_proc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            assert_ne!(RegisterClassExW(&raw const wc), 0, "the class registers");
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                w!("instance test"),
                WS_OVERLAPPED,
                0,
                0,
                10,
                10,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .expect("the window is created")
        };
        let window = hwnd.0 as usize as u64;
        let this = Running {
            hwnd,
            pid: std::process::id(),
        };

        let (_dir, lock) = fresh_lock();
        assert_eq!(claim(&lock), Claim::Ours);
        assert_eq!(running(&lock), None, "an instance with no window yet");
        mark_window(hwnd);
        assert_eq!(running(&lock), Some(this));
        // A record that names another process for the window, as a
        // stale one would once the window's handle was given out again.
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            let stale = Record {
                pid: std::process::id().wrapping_add(4),
                hwnd: window,
            };
            write_record(held.as_mut().expect("a lock is held"), stale).expect("the record");
        });
        assert_eq!(
            running(&lock),
            None,
            "a window that is not the named process's"
        );
        record_window(window);
        assert_eq!(running(&lock), Some(this));

        // A window that answers "later" until the start's time is up:
        // its queue is full.
        QUEUED.with(|q| q.borrow_mut().extend((0..MAX_QUEUED).map(|_| None)));
        let Start::Refused(why) = start_while_pumping(&lock, None, Duration::from_millis(200))
        else {
            panic!("a start that was never taken says it was, or starts beside the instance");
        };
        assert!(
            why.contains("not responding"),
            "the instance was found, and the user is told another program may be: {why}"
        );
        QUEUED.with(|q| q.borrow_mut().clear());
        BRING_FORWARD.with(|f| f.set(false));

        let path = PathBuf::from(r"C:\handed\over.txt");
        let started = start_while_pumping(&lock, Some(&path), Duration::from_secs(20));
        assert!(matches!(started, Start::Forwarded), "the window took it");
        let queued: Vec<_> = QUEUED.with(|q| q.borrow_mut().drain(..).collect());
        assert_eq!(queued, vec![Some(path)]);
        assert!(
            BRING_FORWARD.with(|f| f.replace(false)),
            "and comes to the front"
        );

        unmark_window();
        assert_eq!(
            read_record(&lock),
            Some(Record {
                pid: std::process::id(),
                hwnd: 0
            }),
            "a window that is going is taken out of the lock"
        );
        assert_eq!(running(&lock), None);
        release();
        // SAFETY: the window this test created, on its thread.
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}

#[cfg(test)]
mod wiring_guards {
    //! Where the pieces above are called from, pinned in the source.
    //! Each order below compiles either way and looks right in a diff,
    //! and what it decides needs two processes and a quit to observe.

    use crate::plugin_reentry_guards::{block_after, code_only, fn_body, production_src};

    /// This file's code before its first test module, comments removed.
    fn instance_src() -> String {
        let src = include_str!("instance.rs");
        let end = src.find("#[cfg(test)]").unwrap_or(src.len());
        code_only(&src[..end])
    }

    fn at(haystack: &str, needle: &str) -> usize {
        haystack
            .find(needle)
            .unwrap_or_else(|| panic!("no `{needle}`"))
    }

    /// A second start ends before it has made a window, registered a
    /// class or read a session: all of that belongs to the instance.
    #[test]
    fn a_start_is_decided_before_anything_is_created() {
        let run = code_only(&fn_body(production_src(), "run"));
        let start = at(&run, "instance::start(");
        for later in [
            "GetModuleHandleW(",
            "RegisterClassExW(",
            "CreateWindowExW(",
            "Shell::new(",
        ] {
            assert!(
                start < at(&run, later),
                "`{later}` runs before the start is decided"
            );
        }
        assert!(
            run[start..].starts_with("instance::start(initial_path.as_deref())"),
            "the start no longer hands over the file named on the command line"
        );
        assert!(
            at(&run, "std::path::absolute(") < start,
            "the path is handed over as written: the instance would read a relative one \
             from its own working folder"
        );
    }

    /// The window can be found from the moment it exists, so that a
    /// start made while this one is still starting waits for it rather
    /// than for its time to run out.
    #[test]
    fn the_window_is_marked_before_the_session_is_restored() {
        let run = code_only(&fn_body(production_src(), "run"));
        let mark = at(&run, "instance::mark_window(main_hwnd)");
        // The main window's own style: its `CreateWindowExW`, not the
        // class registration that names the class first.
        assert!(
            at(&run, "WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,") < mark,
            "marked before the window exists"
        );
        assert!(mark < at(&run, "load_session_entries()"));
    }

    /// The session first, then the file named on the command line, so
    /// that a start with a file does not replace the session with it.
    #[test]
    fn a_start_with_a_file_restores_the_session_first() {
        let run = code_only(&fn_body(production_src(), "run"));
        let restore = at(&run, "load_session_entries()");
        let named = at(&run, "if let Some(path) = initial_path {");
        assert!(
            restore < named,
            "the named file is opened before the session"
        );
        // Before the restore the path is named three times: made
        // absolute (twice on that line) and handed to the start. A
        // condition on it, in any spelling, would be a fourth.
        assert_eq!(
            run[..restore].matches("initial_path").count(),
            3,
            "something before the session restore looks at the file named on the command \
             line; the session is restored whether or not there is one"
        );
    }

    /// The lock goes after the session is saved, never before: the
    /// start that takes it next reads that session at once.
    #[test]
    fn the_next_start_is_let_in_only_once_the_session_is_saved() {
        let destroy = code_only(&block_after(production_src(), "WM_DESTROY => {"));
        let save = at(&destroy, "shell.save_session(");
        let release = at(&destroy, "instance::release();");
        let restart = at(&destroy, "instance::restart_for_unopened();");
        assert!(
            save < release,
            "the lock is let go before the session is saved"
        );
        assert!(
            release < restart,
            "Code++ is started again while this one still holds the lock"
        );
        assert!(
            at(&destroy, "instance::unmark_window();") < save,
            "a start can still find the window while it is being torn down"
        );
    }

    /// A sent message is dispatched wherever the UI thread happens to
    /// be, with whatever state borrow is live below it.
    #[test]
    fn receiving_touches_no_window_state() {
        let src = instance_src();
        // Nothing of the window's at all: the one thing either may ask
        // the rest of the crate is whether a quit was asked for.
        let on_copydata = fn_body(&src, "on_copydata");
        assert_eq!(
            on_copydata.matches("super::").count(),
            1,
            "`on_copydata` reaches into the window's code"
        );
        assert!(
            on_copydata.contains("!super::quit_requested()"),
            "a window asked to quit still takes a start it will never open"
        );
        assert!(
            !fn_body(&src, "receive").contains("super::"),
            "`receive` reaches into the window's code"
        );
    }

    /// Opened as the user's own gesture, and only when the window is
    /// free to take one.
    #[test]
    fn a_handed_over_file_is_opened_when_the_window_is_quiet() {
        let src = instance_src();
        let body = fn_body(&src, "on_forwarded");
        let quitting = at(&body, "if super::quitting() {");
        let quiet = at(&body, "if !unsafe { quiet(hwnd) } {");
        let open = at(&body, "super::open_user_path(hwnd, path)");
        assert!(quitting < quiet && quiet < open);
        assert!(
            at(&body, "iter().all(Option::is_none)") < quiet,
            "the retry timer is set with nothing to open"
        );
        let quitting_arm = block_after(&body, "if super::quitting() {");
        assert!(
            !quitting_arm.contains("QUEUED"),
            "a quit empties the queue; `restart_for_unopened` then has nothing to start for"
        );
        let quiet = fn_body(&src, "quiet");
        for term in [
            "IsWindowEnabled(hwnd)",
            "!in_menu_or_sizing()",
            "!super::DrainFreeze::active()",
            "!super::STARTUP_RESTORE.with(Cell::get)",
            "codepp_plugin_host::calling_plugin().is_none()",
            "super::state_from_hwnd(hwnd)",
        ] {
            assert!(quiet.contains(term), "`quiet` no longer checks `{term}`");
        }
    }
}
