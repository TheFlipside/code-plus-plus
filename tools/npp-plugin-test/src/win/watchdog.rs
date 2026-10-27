//! A second thread that keeps a run from hanging.
//!
//! A plugin that puts up a message box during its load blocks the UI
//! thread in the box's modal loop until someone answers it — the real
//! `NppExec`'s "requires Notepad++ 5.1 or higher" did exactly that when
//! Code++ answered `NPPM_GETNPPVERSION` from inside a borrow. Under CI
//! nobody would, so the run would hang. The watchdog watches the step
//! the UI thread is in. Once a step has run longer than [`GRACE`], a
//! top-level window of the UI thread's that has been up for at least
//! [`WINDOW_GRACE`], that the host did not make and that the plugin did
//! not register as a dock panel or a modeless dialog, is taken for a
//! dialog the step is waiting on: the watchdog records it and asks it to
//! close. A window that comes and goes while the step finishes promptly
//! is a plugin's own business and is left alone.
//!
//! A window counts as up when it has the `WS_VISIBLE` style, read from
//! the window itself. `IsWindowVisible` would also ask the desktop
//! window, and on a runner with no interactive desktop that reads as
//! hidden for every window (DEVELOPMENT.md §2.7).
//!
//! Everything it does to another thread's window is non-blocking:
//! `InternalGetWindowText` rather than `GetWindowTextW` (which sends
//! `WM_GETTEXT` and would wait on a thread that is not pumping), style
//! reads, and `PostMessageW` rather than `SendMessageW`. So a plugin
//! spinning without pumping cannot hang the watchdog too, and the hard
//! timeout — a step taking longer than the run's `--timeout` — always
//! fires: it prints what it knows and ends the process with status 3.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumThreadWindows, GetClassNameW, GetDlgItem, GetWindowLongPtrW, InternalGetWindowText,
    IsWindow, PostMessageW, GWL_STYLE, IDCANCEL, IDNO, IDOK, WM_CLOSE, WM_COMMAND, WS_VISIBLE,
};

use crate::report::DialogReport;

/// How long a step may run with a foreign window up before the
/// watchdog takes it for a dialog the step is waiting on.
pub const GRACE: Duration = Duration::from_millis(1500);

/// How long a window must have been up before the watchdog acts on it,
/// so one that opened a moment ago and is about to close is not taken
/// for a dialog the step waits on.
pub const WINDOW_GRACE: Duration = Duration::from_secs(1);

/// How often the watchdog looks.
const TICK: Duration = Duration::from_millis(100);

/// How many times a window is asked to close before the watchdog leaves
/// it to the timeout: Close, Cancel, No, then OK a few more times.
const MAX_ATTEMPTS: u32 = 6;

/// The control id `MessageBoxW` gives its text.
const MESSAGE_BOX_TEXT_ID: i32 = 0xFFFF;

/// The exit status of a run that timed out.
pub const TIMEOUT_EXIT_CODE: i32 = 3;

/// The step the UI thread is in, and when it began.
struct Phase {
    name: String,
    /// A `--run` command, which may open a dialog on purpose.
    command: bool,
    started: Instant,
}

/// What the UI thread and the watchdog share.
struct Shared {
    phase: Mutex<Phase>,
    /// Top-level windows a plugin registered with the host (dock
    /// panels, modeless dialogs): not dialogs it is waiting on.
    allowed: Mutex<HashSet<isize>>,
    dialogs: Mutex<Vec<(isize, DialogReport)>>,
    stop: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The running watchdog. Dropping it without [`Self::stop`] leaves the
/// thread to die with the process.
pub struct Watchdog {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    /// Start watching the UI thread `ui_thread`, whose host window is
    /// `host`, giving up on any one step after `timeout`. `None` when the
    /// thread could not be started.
    #[must_use]
    pub fn start(ui_thread: u32, host: isize, timeout: Duration) -> Option<Self> {
        let shared = Arc::new(Shared {
            phase: Mutex::new(Phase {
                name: "start".to_string(),
                command: false,
                started: Instant::now(),
            }),
            allowed: Mutex::new(HashSet::new()),
            dialogs: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        });
        let watched = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("npp-plugin-test watchdog".to_string())
            .spawn(move || {
                let survived = catch_unwind(AssertUnwindSafe(|| {
                    watch(&watched, ui_thread, host, timeout);
                }));
                if survived.is_err() {
                    // Nothing bounds the run any more; end it rather than
                    // risk an unattended hang.
                    end_run("the watchdog panicked; the run can no longer be bounded");
                }
            })
            .ok()?;
        Some(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Enter the step `name` — a `--run` command when `command` —
    /// restarting its clock.
    pub fn phase(&self, name: String, command: bool) {
        let mut phase = lock(&self.shared.phase);
        phase.name = name;
        phase.command = command;
        phase.started = Instant::now();
    }

    /// A handle for marking windows a plugin registered.
    #[must_use]
    pub fn allow_list(&self) -> AllowList {
        AllowList(Arc::clone(&self.shared))
    }

    /// The windows found so far, without stopping.
    #[must_use]
    pub fn dialogs_so_far(&self) -> Vec<DialogReport> {
        lock(&self.shared.dialogs)
            .iter()
            .map(|(_, dialog)| dialog.clone())
            .collect()
    }

    /// Stop watching and hand back every window it found, in the order
    /// found — the ones [`Self::dialogs_so_far`] gave first.
    #[must_use]
    pub fn stop(mut self) -> Vec<DialogReport> {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        lock(&self.shared.dialogs)
            .drain(..)
            .map(|(_, dialog)| dialog)
            .collect()
    }
}

/// Lets the host record that a top-level window is a plugin's
/// registered panel or dialog rather than one it is blocked on.
#[derive(Clone)]
pub struct AllowList(Arc<Shared>);

impl AllowList {
    /// Mark `hwnd` as registered (`true`) or no longer (`false`).
    pub fn set(&self, hwnd: isize, allowed: bool) {
        let mut set = lock(&self.0.allowed);
        if allowed {
            set.insert(hwnd);
        } else {
            set.remove(&hwnd);
        }
    }
}

/// What the watchdog knows of one window it has seen up.
struct Seen {
    since: Instant,
    attempts: u32,
}

fn watch(shared: &Shared, ui_thread: u32, host: isize, timeout: Duration) {
    let mut seen: HashMap<isize, Seen> = HashMap::new();
    while !shared.stop.load(Ordering::Acquire) {
        std::thread::sleep(TICK);
        let (name, command, elapsed) = {
            let phase = lock(&shared.phase);
            (phase.name.clone(), phase.command, phase.started.elapsed())
        };
        if elapsed > timeout {
            end_run(&format!("{name:?} took longer than {}s", timeout.as_secs()));
        }
        // A window that has gone was dismissed, and its handle may be
        // reused by a new window, which must start afresh. A handle
        // reused within one tick is not told apart; `MAX_ATTEMPTS` still
        // bounds what such a window receives.
        // SAFETY: `IsWindow` takes any value and only reads.
        seen.retain(|&hwnd, _| unsafe { IsWindow(Some(hwnd_from(hwnd))) }.as_bool());
        for (hwnd, dialog) in lock(&shared.dialogs).iter_mut() {
            if !dialog.dismissed && !seen.contains_key(hwnd) {
                dialog.dismissed = true;
            }
        }
        // A registered window that has gone frees its handle for reuse,
        // possibly by a dialog the step is blocked on.
        // SAFETY: `IsWindow` takes any value and only reads.
        lock(&shared.allowed).retain(|&hwnd| unsafe { IsWindow(Some(hwnd_from(hwnd))) }.as_bool());
        let allowed = lock(&shared.allowed).clone();
        let now = Instant::now();
        for hwnd in shown_top_level_windows(ui_thread) {
            if hwnd == host || allowed.contains(&hwnd) {
                continue;
            }
            let window = seen.entry(hwnd).or_insert(Seen {
                since: now,
                attempts: 0,
            });
            if elapsed < GRACE
                || now.duration_since(window.since) < WINDOW_GRACE
                || window.attempts >= MAX_ATTEMPTS
            {
                continue;
            }
            if window.attempts == 0 {
                lock(&shared.dialogs).push((hwnd, describe(hwnd, &name, command)));
            }
            dismiss(hwnd, window.attempts);
            window.attempts += 1;
        }
    }
}

/// Print why the run is ending and end the process with
/// [`TIMEOUT_EXIT_CODE`]. The CLI has already printed the report's
/// header, and perhaps its body, so this adds lines to whatever is out.
fn end_run(why: &str) -> ! {
    // The main thread may hold stdout, blocked writing to a reader that
    // stopped reading; the run must end regardless.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(2));
        std::process::exit(TIMEOUT_EXIT_CODE);
    });
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "timeout: {}", crate::report::clean(why));
    let _ = writeln!(out, "verdict: TIMEOUT");
    let _ = out.flush();
    std::process::exit(TIMEOUT_EXIT_CODE);
}

fn hwnd_from(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

/// The top-level windows the UI thread owns that have `WS_VISIBLE`.
fn shown_top_level_windows(ui_thread: u32) -> Vec<isize> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Vec` below, borrowed for the
        // synchronous enumeration.
        let found = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
        // SAFETY: `hwnd` comes from the enumeration; reading a style
        // sends nothing.
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
        if style & isize::try_from(WS_VISIBLE.0).unwrap_or(0) != 0 {
            found.push(hwnd.0 as isize);
        }
        BOOL(1)
    }
    let mut found: Vec<isize> = Vec::new();
    // SAFETY: `collect` only touches `found`, which outlives the call;
    // `EnumThreadWindows` is synchronous and sends nothing to the
    // enumerated windows.
    unsafe {
        let _ = EnumThreadWindows(
            ui_thread,
            Some(collect),
            LPARAM(std::ptr::addr_of_mut!(found) as isize),
        );
    }
    found
}

/// A window's class, caption and — for a message box — text, read
/// without sending it anything.
fn describe(hwnd: isize, phase: &str, in_command: bool) -> DialogReport {
    let h = hwnd_from(hwnd);
    let mut buf = [0u16; 512];
    // SAFETY: `h` names a window of this process (it came from the
    // enumeration); `GetClassNameW` only reads, into `buf`, and sends
    // nothing.
    let class = unsafe {
        let n = GetClassNameW(h, &mut buf);
        String::from_utf16_lossy(&buf[..usize::try_from(n).unwrap_or(0)])
    };
    let title = internal_text(h);
    // SAFETY: as above; `GetDlgItem` only reads.
    let text = unsafe { GetDlgItem(Some(h), MESSAGE_BOX_TEXT_ID) }
        .map(internal_text)
        .unwrap_or_default();
    DialogReport {
        phase: phase.to_string(),
        in_command,
        class,
        title,
        text,
        dismissed: false,
    }
}

fn internal_text(h: HWND) -> String {
    let mut buf = vec![0u16; 4096];
    // SAFETY: `InternalGetWindowText` reads the window's cached text
    // into `buf` without sending a message.
    let n = unsafe { InternalGetWindowText(h, &mut buf) };
    String::from_utf16_lossy(&buf[..usize::try_from(n).unwrap_or(0)])
}

/// Ask a window to go away, a little more firmly each time: close it,
/// then press Cancel, No and OK in turn — a `MB_YESNO` box ignores both
/// a close and Cancel. So an unattended run may accept a prompt;
/// `--help` says so.
fn dismiss(hwnd: isize, attempt: u32) {
    let (msg, wparam) = dismissal(attempt);
    // SAFETY: posting only enqueues; a window that has gone fails the
    // post, which is ignored.
    unsafe {
        let _ = PostMessageW(Some(hwnd_from(hwnd)), msg, WPARAM(wparam), LPARAM(0));
    }
}

/// The message, and its `wParam`, of the `attempt`th request to close.
fn dismissal(attempt: u32) -> (u32, usize) {
    let command = |id: i32| usize::try_from(id).unwrap_or(0);
    match attempt {
        0 => (WM_CLOSE, 0),
        1 => (WM_COMMAND, command(IDCANCEL.0)),
        2 => (WM_COMMAND, command(IDNO.0)),
        _ => (WM_COMMAND, command(IDOK.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dismissal_escalates_from_close_to_ok() {
        assert_eq!(dismissal(0), (WM_CLOSE, 0));
        assert_eq!(dismissal(1), (WM_COMMAND, 2)); // IDCANCEL
        assert_eq!(dismissal(2), (WM_COMMAND, 7)); // IDNO
        assert_eq!(dismissal(3), (WM_COMMAND, 1)); // IDOK
        assert_eq!(dismissal(MAX_ATTEMPTS - 1), (WM_COMMAND, 1));
    }

    #[test]
    fn the_step_and_window_graces_are_shorter_than_the_default_timeout() {
        // Else a dialog would end the run by timeout before the watchdog
        // ever answered it.
        assert!(GRACE + WINDOW_GRACE + TICK * MAX_ATTEMPTS < crate::options::DEFAULT_TIMEOUT);
    }
}
