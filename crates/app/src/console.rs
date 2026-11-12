//! The console Code++ writes its diagnostics to, on Windows.
//!
//! # Why this exists
//!
//! `codepp.exe` is linked for the Windows GUI subsystem (the
//! `windows_subsystem` attribute at the top of `main.rs`). A program
//! linked for the console subsystem is given a console by Windows
//! whenever it is started from somewhere that has none — Explorer, the
//! Start menu, a shortcut, a file association — so every such start
//! opened a console window beside the editor, which stayed until the
//! editor closed. The first alpha shipped that way.
//!
//! A GUI-subsystem program gets no console at all, not even the one of
//! the terminal it was started from. Its standard handles are whatever
//! the parent passed: a file or a pipe where the output is redirected,
//! and nothing otherwise (measured: all three are NULL when started
//! from `cmd` with no redirect). So `codepp --help` typed into a
//! terminal would print nowhere. [`attach`] joins the parent's console
//! for the runs that have something to say.
//!
//! # Why not on every start
//!
//! A process attached to a console belongs to it: Ctrl+C in that
//! terminal, or closing its window, ends every process attached. An
//! editor started with `codepp notes.txt` must outlive the terminal it
//! was typed into, so a plain start attaches to nothing. Only a run
//! that asked for output (`--help`, a bad argument, `--verbose`,
//! `--perf`, `CODEPP_LOG`) or one that is about to exit with an error
//! attaches, and such a run was tied to its terminal before as well.
//!
//! A panic is such an error. Its message goes to stderr, which a plain
//! start does not have, so [`attach_on_panic`] attaches before the
//! message is written. A release build aborts on a panic, so nothing is
//! left to tie to the terminal; a debug build that catches one at a
//! callback boundary runs on attached, which a developer's run can
//! afford.
//!
//! # Redirected output stays redirected
//!
//! `AttachConsole` can replace standard handles the process already
//! has. Measured on Windows 11 with a probe program: started by `cmd`
//! with `> out.txt 2> err.txt`, the probe's handles named the two
//! files before the call and the console after it, and both files
//! stayed empty. `cmd` hands its redirections down by inheritance; a
//! parent that passes them in the start-up information, as Rust's
//! `Command` and Git Bash do, keeps them. So [`attach`] puts back every
//! handle the process was started with, and the console fills in only
//! the ones it was started without. With that, each case measured
//! writes where the command line says: both streams to files, one
//! stream to a file and the other to the terminal, `2>NUL`, a pipe,
//! and no redirect at all.
//!
//! Off Windows there is nothing to do: a process shares its parent's
//! terminal by inheriting its file descriptors.

/// Join the console of the process that started this one, if it has
/// one, so that what is written to stdout and stderr after this call
/// shows there.
///
/// Call it before the first write of a run that has something to
/// report. It does nothing where there is nothing to do: off Windows,
/// on a second call, and when the parent has no console (a start from
/// Explorer). Output the parent redirected to a file or a pipe keeps
/// going there: see the module docs.
pub fn attach() {
    #[cfg(windows)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(windows::attach_keeping_redirects);
    }
}

/// Have a panic join the parent's console before its message is
/// written, so that a crash of an editor started plainly from a
/// terminal says why (module docs). The hook that was installed, std's
/// own unless something replaced it, still writes the message.
///
/// Call it once, first thing. Off Windows it does nothing: stderr is
/// the terminal's there already.
pub fn attach_on_panic() {
    #[cfg(windows)]
    {
        let report = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            attach();
            report(info);
        }));
    }
}

#[cfg(windows)]
mod windows {
    //! Declared here rather than taken from the `windows` crate: these
    //! are the only Win32 calls the binary makes itself, and `kernel32`
    //! is linked into every Windows program already.

    /// A Win32 `HANDLE`, as the integer it is passed as.
    type Handle = isize;

    /// `AttachConsole`'s process id for the parent of the calling
    /// process (`(DWORD)-1`, from `wincon.h`).
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;

    /// `STD_INPUT_HANDLE`, `STD_OUTPUT_HANDLE` and `STD_ERROR_HANDLE`
    /// (`(DWORD)-10`, `-11` and `-12`, from `processenv.h`).
    const STD_HANDLES: [u32; 3] = [0xFFFF_FFF6, 0xFFFF_FFF5, 0xFFFF_FFF4];

    /// `INVALID_HANDLE_VALUE`, which `GetStdHandle` answers on failure.
    const INVALID_HANDLE_VALUE: Handle = -1;

    #[link(name = "kernel32")]
    extern "system" {
        /// `BOOL AttachConsole(DWORD dwProcessId)`.
        fn AttachConsole(process_id: u32) -> i32;
        /// `HANDLE GetStdHandle(DWORD nStdHandle)`.
        fn GetStdHandle(std_handle: u32) -> Handle;
        /// `BOOL SetStdHandle(DWORD nStdHandle, HANDLE hHandle)`.
        fn SetStdHandle(std_handle: u32, handle: Handle) -> i32;
    }

    /// Whether the process was started with `handle` as a standard
    /// handle: `GetStdHandle` answers NULL for one it was started
    /// without.
    pub(super) fn was_given(handle: Handle) -> bool {
        handle != 0 && handle != INVALID_HANDLE_VALUE
    }

    /// Attach to the parent's console, then put back the standard
    /// handles the process was started with, which the attach may have
    /// replaced (module docs).
    pub(super) fn attach_keeping_redirects() {
        // SAFETY: `GetStdHandle` takes one of the three documented ids
        // by value and reads no memory of ours.
        let given = STD_HANDLES.map(|id| unsafe { GetStdHandle(id) });

        // SAFETY: `AttachConsole` takes a process id by value and reads
        // no memory of ours; `ATTACH_PARENT_PROCESS` is the documented
        // id for the parent of the current process.
        let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0;
        if !attached {
            // No parent console (a start from Explorer), or one is
            // attached already. The process is as it was.
            return;
        }

        for (id, handle) in STD_HANDLES.into_iter().zip(given) {
            if was_given(handle) {
                // SAFETY: `handle` is the value `GetStdHandle` answered
                // for this same id a moment ago; nothing has closed it
                // since, and `SetStdHandle` only records it. A failure
                // leaves the console's handle in place, so the output
                // shows on the terminal rather than nowhere.
                unsafe {
                    SetStdHandle(id, handle);
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{was_given, INVALID_HANDLE_VALUE};

        #[test]
        fn only_a_real_handle_counts_as_given() {
            assert!(!was_given(0), "NULL: started without this handle");
            assert!(!was_given(INVALID_HANDLE_VALUE));
            assert!(was_given(0x70));
        }
    }
}
