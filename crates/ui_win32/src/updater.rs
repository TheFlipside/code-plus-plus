//! The Win32 window's side of the auto-updater (DESIGN.md §11).
//!
//! The updater itself is `codepp_shell::updater`, a worker thread the
//! window never waits for. What is here is where the window touches it:
//!
//! * [`paint_asks`] and [`start_if_asked`] start the worker, once
//!   a frame is on screen. Nothing of the updater runs before that
//!   (§11.2).
//! * [`refresh_menu_entry`] greys "?" → "Update Code++" until the worker
//!   has established that this copy can update itself (§11.5).
//! * [`stop_at_quit`] stops the updater's library as the window goes,
//!   and keeps a verified update aside.
//! * [`launch_ready_update`] starts that update's installer, as the
//!   last thing `run` does: the session is saved, the instance lock let
//!   go and the window destroyed by then, and the process ends straight
//!   after, which Windows Installer needs to replace `codepp.exe`
//!   (§11.4).

use std::cell::{Cell, RefCell};

use codepp_shell::updater::ReadyInstall;
use codepp_shell::Shell;
use windows::Win32::UI::WindowsAndMessaging::{
    EnableMenuItem, HMENU, MENU_ITEM_FLAGS, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED,
};

/// Where the start of the updater's worker stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    /// No frame has been drawn yet.
    BeforeFirstPaint,
    /// A frame is drawn, and the worker is to be started by the next
    /// wake that reaches the window's state.
    Asked,
    /// The worker is started.
    Done,
}

thread_local! {
    static START: Cell<Start> = const { Cell::new(Start::BeforeFirstPaint) };

    /// The verified update the quit found waiting, between
    /// [`stop_at_quit`], which runs while the window's state still
    /// exists, and [`launch_ready_update`], which runs when it no
    /// longer does. It holds the installer's file open, denying writes.
    static READY_UPDATE: RefCell<Option<ReadyInstall>> = const { RefCell::new(None) };
}

/// Called after each paint of the main editor: true until the worker
/// is started, and the caller then posts a wake for [`start_if_asked`].
///
/// The paint notification only asks. It arrives from inside Scintilla,
/// possibly below a frame that has the window's state borrowed, so it
/// must not take the shell itself; the wake handler is where the shell
/// is drained, and starts the worker there. It asks at every paint
/// until that has happened, and not once: a wake can reach the handler
/// while the state cannot be had, during a call into a plugin say, and
/// asking once would then leave the worker unstarted for the session.
/// Usually the first wake starts it, and every later paint costs one
/// comparison.
pub(crate) fn paint_asks() -> bool {
    START.with(|start| match start.get() {
        Start::BeforeFirstPaint => {
            start.set(Start::Asked);
            true
        }
        Start::Asked => true,
        Start::Done => false,
    })
}

/// Start the updater's worker if a first paint has asked for it and it
/// is not started yet. Called from the wake handler, ahead of its
/// drain; every later call is one comparison.
pub(crate) fn start_if_asked(shell: &mut Shell) {
    START.with(|start| {
        if start.get() == Start::Asked {
            start.set(Start::Done);
            shell.start_updater();
        }
    });
}

/// Enable or grey the "Update Code++" entry `id` in `menu`, by whether
/// this copy can update itself. Called as a menu opens; a menu that
/// does not hold the entry is left as it is.
///
/// # Safety
///
/// `menu` must be a live `HMENU`; UI thread only.
pub(crate) unsafe fn refresh_menu_entry(menu: HMENU, id: u16, shell: &Shell) {
    let state = if shell.can_self_update() {
        MF_ENABLED
    } else {
        MF_GRAYED
    };
    // SAFETY: the caller's contract. A menu without the entry answers
    // -1 and changes nothing.
    unsafe {
        let _ = EnableMenuItem(
            menu,
            u32::from(id),
            MENU_ITEM_FLAGS(MF_BYCOMMAND.0 | state.0),
        );
    }
}

/// Stop the updater's library and keep a verified update for
/// [`launch_ready_update`]. Called from `WM_DESTROY`, after the session
/// is saved and the instance lock let go: stopping the library waits a
/// moment for its threads, which must not hold up either.
///
/// `restarted` says that Code++ has just been started again, for files
/// another start handed over that the quit kept from opening. The
/// update is then let go: that Code++ is running `codepp.exe`, which
/// the installer could not replace, and a file the user asked for is
/// not dropped for an update that will be offered again.
pub(crate) fn stop_at_quit(shell: &mut Shell, restarted: bool) {
    shell.shut_down_updater();
    let ready = shell.take_ready_update();
    if restarted {
        if let Some(ready) = ready {
            tracing::warn!(
                version = ready.version(),
                "a verified update was not installed: Code++ was started again for files \
                 handed over during the quit"
            );
        }
        return;
    }
    READY_UPDATE.with(|slot| *slot.borrow_mut() = ready);
}

/// Start the installer of the update [`stop_at_quit`] kept, if there is
/// one. Called once the message loop has ended, as the last thing
/// before the process exits. A failure can only be logged: no window is
/// left to say it in, and the file is let go either way.
pub(crate) fn launch_ready_update() {
    let Some(ready) = READY_UPDATE.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    let version = ready.version().to_owned();
    match ready.launch() {
        Ok(pid) => tracing::info!(version, pid, "update installer started"),
        Err(error) => {
            tracing::error!(version, error = ?error.to_string(), "update installer could not be started");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paint_asks_until_the_worker_is_started_and_not_after() {
        // Each test thread has its own state.
        assert!(START.with(Cell::get) == Start::BeforeFirstPaint);
        assert!(paint_asks());
        assert!(START.with(Cell::get) == Start::Asked);
        // A wake that could not reach the state started nothing, so the
        // next paint asks again.
        assert!(paint_asks());
        // Started: `start_if_asked` leaves this behind.
        START.with(|start| start.set(Start::Done));
        assert!(!paint_asks());
        assert!(!paint_asks());
        assert!(START.with(Cell::get) == Start::Done);
    }
}
