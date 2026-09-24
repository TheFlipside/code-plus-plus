//! Which plugin's code the host is running on this thread.
//!
//! `SendMessage` carries no sender, so a message a plugin sends the host
//! does not say which plugin sent it. The host can still tell in the
//! cases that matter. Nearly every time a plugin runs, it runs because
//! the host called it — its load entry points, a notification, one of
//! its menu commands, an inter-plugin message — and whatever it sends
//! the host from there arrives on the same thread before that call
//! returns. Each of those call sites marks the plugin it is calling for
//! the length of the call ([`CallingPlugin::enter`]), and a handler
//! reads the mark ([`calling_plugin`]).
//!
//! What it is for: `NPPM_DMMREGASDCKDLG` names a module in
//! `tTbData.pszModuleName`, and Code++ signs a panel's startup command
//! only when the plugin registering the panel is the plugin that name
//! identifies — so one plugin cannot pick which of another's commands
//! runs at startup (`codepp_core::dock::CommandSeal`).
//!
//! What it is not is a boundary against a plugin set on getting round
//! it (DESIGN.md §6.5). A message sent from anywhere else — a plugin's
//! own window procedure, a timer, another thread — carries no mark, and
//! is treated as unattributed. One sent from inside a nested message
//! loop that some plugin's call is running carries *that* plugin's mark:
//! a plugin whose timer fires while another plugin's modal dialog is up
//! is taken for the plugin that opened the dialog. Enough to catch a
//! plugin declaring another's name by mistake, and to make doing it on
//! purpose take some effort; a plugin determined to do it runs in the
//! host's process and can do anything the host can.

use std::cell::Cell;

use crate::ffi::PluginCmd;

thread_local! {
    /// The registry index marked by the innermost live
    /// [`CallingPlugin`] on this thread.
    static CALLING: Cell<Option<usize>> = const { Cell::new(None) };
}

/// The registry index of the plugin whose code the host is running on
/// this thread, or `None` outside any call the host made into a plugin.
#[must_use]
pub fn calling_plugin() -> Option<usize> {
    CALLING.with(Cell::get)
}

/// Marks a plugin as the one being called, until dropped.
///
/// Dropping puts back whatever was marked before, so a nested call — a
/// plugin's command sending another plugin's command — is attributed to
/// the inner plugin while it runs and to the outer one again afterwards,
/// and a panic unwinding out of the inner call does not leave the wrong
/// plugin marked.
#[must_use = "the mark lasts only as long as the guard; bind it to a named variable"]
pub struct CallingPlugin {
    previous: Option<usize>,
}

impl CallingPlugin {
    /// Mark the plugin at registry index `idx` as the one being called.
    pub fn enter(idx: usize) -> Self {
        Self {
            previous: CALLING.with(|c| c.replace(Some(idx))),
        }
    }
}

impl Drop for CallingPlugin {
    fn drop(&mut self) {
        CALLING.with(|c| c.set(self.previous));
    }
}

/// A plugin's menu command, with the plugin it belongs to.
///
/// Its fields are private to this crate so that [`Self::run`] is the
/// only way to call it from outside: a backend cannot reach the raw
/// function pointer and forget the mark.
#[derive(Clone, Copy, Debug)]
pub struct PluginCommand {
    /// Registry index of the plugin whose `FuncItem` this is.
    pub(crate) owner: usize,
    /// The `FuncItem`'s `p_func`.
    pub(crate) func: PluginCmd,
}

impl PluginCommand {
    /// Run the command as its plugin, marked with [`CallingPlugin`] for
    /// the length of the call.
    ///
    /// # Safety
    ///
    /// Call on the UI thread, as the Notepad++ ABI requires, while the
    /// `PluginHost` the command came from is alive — plugins are never
    /// unloaded before it drops, so its library is mapped until then.
    pub unsafe fn run(self) {
        let _calling = CallingPlugin::enter(self.owner);
        // SAFETY: forwarded from the caller; `func` takes no arguments.
        unsafe { (self.func)() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scopes nest: the innermost call is the one reported, the outer
    /// one again once it returns, and nothing outside every call.
    #[test]
    fn marks_nest_and_unwind() {
        assert_eq!(calling_plugin(), None);
        {
            let _outer = CallingPlugin::enter(3);
            assert_eq!(calling_plugin(), Some(3));
            {
                let _inner = CallingPlugin::enter(7);
                assert_eq!(calling_plugin(), Some(7));
            }
            assert_eq!(calling_plugin(), Some(3));
        }
        assert_eq!(calling_plugin(), None);
    }

    /// A panic inside a call does not leave that plugin marked.
    #[test]
    fn a_panic_leaves_the_outer_mark() {
        let _outer = CallingPlugin::enter(1);
        let unwound = std::panic::catch_unwind(|| {
            let _inner = CallingPlugin::enter(2);
            panic!("plugin code");
        });
        assert!(unwound.is_err());
        assert_eq!(calling_plugin(), Some(1));
    }

    /// The mark is per thread: another thread's calls are not this one's.
    #[test]
    fn marks_are_per_thread() {
        let _mine = CallingPlugin::enter(5);
        let theirs = std::thread::spawn(calling_plugin).join().expect("join");
        assert_eq!(theirs, None);
        assert_eq!(calling_plugin(), Some(5));
    }

    thread_local! {
        static SEEN: Cell<Option<usize>> = const { Cell::new(None) };
    }

    unsafe extern "C" fn record_caller() {
        SEEN.with(|s| s.set(calling_plugin()));
    }

    /// A command runs marked as the plugin it belongs to.
    #[test]
    fn a_command_runs_as_its_plugin() {
        let command = PluginCommand {
            owner: 4,
            func: record_caller,
        };
        // SAFETY: `record_caller` is a plain function in this binary.
        unsafe { command.run() };
        assert_eq!(SEEN.with(Cell::get), Some(4));
        assert_eq!(calling_plugin(), None, "and the mark ends with it");
    }
}
