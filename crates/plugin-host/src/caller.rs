//! Which plugin's code the host is running on this thread.
//!
//! A handler for a message a plugin sent sometimes needs to know which
//! plugin sent it. The host keeps one mark per thread for that
//! ([`CallingPlugin::enter`]), which a handler reads
//! ([`calling_plugin`]), and sets it two ways.
//!
//! **While it calls into a plugin, on every platform.** Nearly every
//! time a plugin runs, it runs because the host called it — its load
//! entry points, a notification, one of its menu commands, an
//! inter-plugin message — and whatever it sends the host from there
//! arrives on the same thread before that call returns. Each of those
//! call sites marks the plugin it is calling for the length of the call.
//!
//! **Through the plugin's own route, off Windows.** There a plugin's
//! `SendMessage` reaches the host through the routing callback the host
//! installs into it as it loads (`codepp_plugin_set_dispatch`), and each
//! of the first [`MAX_ROUTED_PLUGINS`] plugins found is given a callback
//! of its own ([`plugin_route`]), which marks the plugin for the length
//! of each message. So every message such a plugin sends on the UI
//! thread carries its mark, wherever it is sent from: a call the host
//! made, or a signal handler, idle or timer of the plugin's own. A route
//! marks a worker thread too, but nothing there reads the mark: an
//! `NPPM_*` sent from one is declined, and an `SCI_*` goes on to the UI
//! thread without it.
//!
//! What it is for: `NPPM_DMMREGASDCKDLG` names a module in
//! `tTbData.pszModuleName`, and Code++ signs a panel's startup command
//! only when the plugin registering the panel is the plugin that name
//! identifies — so one plugin cannot pick which of another's commands
//! runs at startup (`codepp_core::dock::CommandSeal`). And on Linux and
//! macOS a Scintilla widget the host makes for `NPPM_CREATESCINTILLAHANDLE`
//! belongs to the plugin that asked, whose `messageProc` then hears its
//! notifications.
//!
//! What it is not is a boundary against a plugin set on getting round
//! it (DESIGN.md §6.5). On Windows `SendMessage` goes through the OS and
//! says nothing of who sent it, so a message sent from anywhere but a
//! call the host made — a plugin's own window procedure, a timer,
//! another thread — carries no mark, and is treated as unattributed; and
//! one sent from inside a nested message loop that some plugin's call is
//! running carries *that* plugin's mark: a plugin whose timer fires while
//! another plugin's modal dialog is up is taken for the plugin that
//! opened the dialog. Off Windows the first [`MAX_ROUTED_PLUGINS`]
//! plugins found have a route; those beyond them share one callback,
//! which marks none of them and clears a mark a route set, so their
//! messages are never marked more than on Windows: only by the host's
//! calls, and not by one that a routed plugin's message has hidden. And
//! on every platform a plugin runs in the host's process and can do
//! anything the host can, calling another plugin's route among the rest.
//! Enough to catch a plugin declaring another's name by mistake, and to
//! make doing it on purpose take some effort.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::OnceLock;

use crate::ffi::{HostDispatchFn, Hwnd, PluginCmd};

/// What the mark says: the plugin's registry index, and whether the
/// plugin's own route set it rather than a call of the host's.
#[derive(Clone, Copy)]
struct Mark {
    plugin: usize,
    by_route: bool,
}

thread_local! {
    /// The mark set by the innermost live [`CallingPlugin`] on this
    /// thread.
    static CALLING: Cell<Option<Mark>> = const { Cell::new(None) };
}

/// The registry index of the plugin whose code the host is running on
/// this thread: the one it is calling, or — off Windows — the one whose
/// message it is handling. `None` outside both.
#[must_use]
pub fn calling_plugin() -> Option<usize> {
    CALLING.with(Cell::get).map(|mark| mark.plugin)
}

/// Marks a plugin as the one whose code is running — the one the host is
/// calling, or the one whose route a message came in by — until dropped.
///
/// Dropping puts back whatever was marked before, so a nested call — a
/// plugin's command sending another plugin's command — is attributed to
/// the inner plugin while it runs and to the outer one again afterwards,
/// and a panic unwinding out of the inner call does not leave the wrong
/// plugin marked.
///
/// It stays on the thread whose mark it set — dropped on another, it
/// would write its saved mark into that thread's — so it is not `Send`:
///
/// ```compile_fail
/// fn is_send<T: Send>() {}
/// is_send::<codepp_plugin_host::CallingPlugin>();
/// ```
#[must_use = "the mark lasts only as long as the guard; bind it to a named variable"]
pub struct CallingPlugin {
    previous: Option<Mark>,
    /// What makes the guard not `Send`; see the type's doc.
    _same_thread: PhantomData<*const ()>,
}

impl CallingPlugin {
    /// Mark the plugin at registry index `idx` as the one whose code is
    /// running.
    pub fn enter(idx: usize) -> Self {
        Self::replace(Some(Mark {
            plugin: idx,
            by_route: false,
        }))
    }

    /// Mark the plugin at registry index `idx` as the sender of the
    /// message its route is handing on.
    fn enter_by_route(idx: usize) -> Self {
        Self::replace(Some(Mark {
            plugin: idx,
            by_route: true,
        }))
    }

    /// Clear a mark a route set, and keep one a call of the host's set:
    /// for a message from a plugin with no route of its own, which must
    /// not be taken for the plugin whose message was being handled when
    /// it came.
    fn leave_routes() -> Self {
        Self::replace(CALLING.with(Cell::get).filter(|mark| !mark.by_route))
    }

    fn replace(mark: Option<Mark>) -> Self {
        Self {
            previous: CALLING.with(|c| c.replace(mark)),
            _same_thread: PhantomData,
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
    /// Registry index of the plugin this command belongs to — for a
    /// backend deciding whether that plugin may be run at all, as
    /// [`PluginMessageProc::owner`] is for a message.
    #[must_use]
    pub fn owner(self) -> usize {
        self.owner
    }

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

/// One plugin's `messageProc`, with the plugin it belongs to — the
/// [`PluginCommand`] of a message rather than of a menu item.
///
/// For the host messages a plugin receives one at a time rather than by
/// broadcast: an `NPPM_MSGTOPLUGIN` another plugin sent it, and — on
/// the backends with no window procedure to deliver a `WM_NOTIFY` to —
/// the `DMN_*` notifications about its own dock panels. Private fields
/// for the same reason as [`PluginCommand`]'s: [`Self::send`] is the
/// only way to reach the function from outside this crate, so a backend
/// cannot call a plugin's `messageProc` without marking it, and a dock
/// panel the plugin registers from inside the call is then known to be
/// its own.
#[derive(Clone, Copy, Debug)]
pub struct PluginMessageProc {
    /// Registry index of the plugin whose `messageProc` this is.
    pub(crate) owner: usize,
    /// The plugin's exported `messageProc`.
    pub(crate) func: crate::ffi::MessageProcFn,
}

impl PluginMessageProc {
    /// Registry index of the plugin this reaches.
    #[must_use]
    pub fn owner(self) -> usize {
        self.owner
    }

    /// Call the plugin's `messageProc(msg, wparam, lparam)` as that
    /// plugin, marked with [`CallingPlugin`] for the length of the call.
    ///
    /// No `catch_unwind` sits around the call, because it could never
    /// catch anything: `messageProc` is a plain `extern "C"` function,
    /// and a panic cannot unwind out of one — it aborts the process
    /// inside the plugin, before control would return here. The same
    /// holds for [`PluginCommand::run`] and for Win32's `SendMessageW`
    /// to a plugin's window. A wrapper would only suggest a guarantee
    /// that does not exist.
    ///
    /// # Safety
    ///
    /// Call on the UI thread while the `PluginHost` this came from is
    /// alive — plugins are never unloaded before it drops. `wparam` and
    /// `lparam` must be valid for whatever `msg` means to the plugin: a
    /// pointer passed in `lparam` must stay live for the call.
    #[must_use]
    pub unsafe fn send(self, msg: u32, wparam: usize, lparam: isize) -> isize {
        let _calling = CallingPlugin::enter(self.owner);
        // SAFETY: forwarded from the caller; `func` is the plugin's
        // exported `messageProc`, whose C signature this matches.
        unsafe { (self.func)(msg, wparam, lparam) }
    }
}

/// How many plugins have a route of their own: those at registry indices
/// `0..MAX_ROUTED_PLUGINS`, by discovery order. The plugins beyond them
/// share one callback (`unrouted`), which marks none of them, so their
/// messages are never marked more than they are on Windows: only while
/// the host is calling the plugin. A constant because the routes are a
/// table of functions, which a process cannot add to as it runs; far
/// more plugins than anyone installs.
pub const MAX_ROUTED_PLUGINS: usize = 128;

/// The router each route hands its messages to: the backend's
/// [`HostDispatchFn`], set by [`plugin_route`] before the route is given
/// to the plugin, and never changed after.
static ROUTERS: [OnceLock<HostDispatchFn>; MAX_ROUTED_PLUGINS] =
    [const { OnceLock::new() }; MAX_ROUTED_PLUGINS];

/// The route of the plugin at registry index `I`: the routing callback the
/// host installs into that plugin. It marks the plugin as the one whose
/// code is running for the length of the message, then hands the message
/// on to the backend's router unchanged.
///
/// # Safety
///
/// [`HostDispatchFn`]'s contract: what the plugin passes must be valid for
/// `msg`, exactly as for the router it reaches.
unsafe extern "C" fn route<const I: usize>(
    hwnd: Hwnd,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    const { assert!(I < MAX_ROUTED_PLUGINS) };
    // A route is handed out only once its router is set, so this is
    // unreachable; answered as the SDK answers with no callback at all.
    let Some(router) = ROUTERS[I].get().copied() else {
        return 0;
    };
    let _calling = CallingPlugin::enter_by_route(I);
    // SAFETY: the plugin's own arguments, passed on as they came, to the
    // router the backend gave for this plugin — the call the plugin would
    // have made had it been given the router itself.
    unsafe { router(hwnd, msg, wparam, lparam) }
}

/// The router the plugins beyond the routed ones share: the backend's,
/// set by [`plugin_route`] before [`unrouted`] is given to the first of
/// them, and never changed after.
static UNROUTED_ROUTER: OnceLock<HostDispatchFn> = OnceLock::new();

/// The callback the plugins beyond the routed ones share. It cannot say
/// which of them sent a message, so it marks none of them; and it clears
/// a mark a route set — another plugin's message being handled when this
/// one came in, which is not this plugin's — while it keeps one a call of
/// the host's set. Their messages are therefore never marked more than on
/// Windows, where a host call's mark is the only kind; a host call's mark
/// that a routed plugin's message has hidden stays hidden, which errs
/// towards no mark rather than a wrong one.
///
/// # Safety
///
/// [`HostDispatchFn`]'s contract, as for [`route`].
unsafe extern "C" fn unrouted(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize {
    // Handed out only once its router is set; answered as the SDK answers
    // with no callback at all.
    let Some(router) = UNROUTED_ROUTER.get().copied() else {
        return 0;
    };
    let _calling = CallingPlugin::leave_routes();
    // SAFETY: as in `route`: the plugin's own arguments, passed on as they
    // came, to the router the backend gave.
    unsafe { router(hwnd, msg, wparam, lparam) }
}

/// `[route::<0>, route::<1>, …]` from the indices given.
macro_rules! route_table {
    ($($index:tt)*) => {
        [$(route::<$index>),*]
    };
}

/// `ROUTES[i]` is the route of the plugin at registry index `i`. That each
/// index is listed once and in order is what the test walking every route
/// checks.
const ROUTES: [HostDispatchFn; MAX_ROUTED_PLUGINS] = route_table![
    0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15
    16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31
    32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47
    48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63
    64 65 66 67 68 69 70 71 72 73 74 75 76 77 78 79
    80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95
    96 97 98 99 100 101 102 103 104 105 106 107 108 109 110 111
    112 113 114 115 116 117 118 119 120 121 122 123 124 125 126 127
];

/// The routing callback to install into the plugin at registry index
/// `idx` instead of `router`: one of its own, which marks the plugin as
/// the one whose code is running for the length of each message and hands
/// the message on to `router` unchanged. [`crate::execute_load`] installs
/// it; public so a backend's tests can send a message as a plugin would.
///
/// A plugin beyond [`MAX_ROUTED_PLUGINS`] gets the callback those plugins
/// share (`unrouted`), which marks none of them.
///
/// An index has one router for the process — a process has one host, and
/// an index one plugin — so the first router given for an index is the one
/// its route keeps, and likewise for the shared callback. Asked again with
/// a different router, which only a test binary reusing an index can do,
/// this hands that router back unrouted rather than a route that would
/// call the other one.
#[must_use]
pub fn plugin_route(idx: usize, router: HostDispatchFn) -> HostDispatchFn {
    let (slot, route) = if let (Some(slot), Some(route)) = (ROUTERS.get(idx), ROUTES.get(idx)) {
        (slot, *route)
    } else {
        tracing::warn!(
            plugin = idx,
            routes = MAX_ROUTED_PLUGINS,
            "a plugin beyond the routed ones: its messages count as its own only \
             while the host is calling it",
        );
        (&UNROUTED_ROUTER, unrouted as HostDispatchFn)
    };
    let kept = *slot.get_or_init(|| router);
    if std::ptr::fn_addr_eq(kept, router) {
        route
    } else {
        // Only a test binary that offers two routers for one index gets
        // here. It costs that one plugin its route, and the bare router
        // does not clear another plugin's route mark either, so its
        // messages could carry one — what `unrouted` exists to prevent.
        // Both backends pass one router for every load, so production
        // never gets here.
        tracing::error!(
            plugin = idx,
            "this plugin's route already hands to another router; its messages go unrouted",
        );
        router
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

    /// What [`record_message`] saw: the mark, then the three arguments.
    type Seen = (Option<usize>, u32, usize, isize);

    thread_local! {
        static MESSAGE_SEEN: Cell<Option<Seen>> = const { Cell::new(None) };
    }

    /// Records the mark and the arguments rather than asserting on
    /// them: a panic cannot leave an `extern "C"` function, so a failed
    /// assertion in here would abort the test binary instead of failing
    /// the one test.
    unsafe extern "C" fn record_message(msg: u32, wparam: usize, lparam: isize) -> isize {
        MESSAGE_SEEN.with(|s| s.set(Some((calling_plugin(), msg, wparam, lparam))));
        42
    }

    /// A `messageProc` runs marked as its own plugin, gets exactly the
    /// arguments sent, and its answer comes back.
    #[test]
    fn a_message_runs_as_its_plugin() {
        let target = PluginMessageProc {
            owner: 6,
            func: record_message,
        };
        assert_eq!(target.owner(), 6);
        // SAFETY: `record_message` is a plain function in this binary
        // and dereferences nothing.
        let answer = unsafe { target.send(0x004E, 0xABCD, -7) };
        assert_eq!(answer, 42);
        assert_eq!(
            MESSAGE_SEEN.with(Cell::get),
            Some((Some(6), 0x004E, 0xABCD, -7))
        );
        assert_eq!(calling_plugin(), None, "and the mark ends with it");
    }

    /// Asks [`mark_router`] to send a message through the route of the
    /// plugin at `wparam` from inside the one it is handling.
    const NEST: u32 = 1;

    thread_local! {
        /// The mark the nested route of a [`NEST`] message saw.
        static INNER: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// The mark, as an answer a router can return: the plugin's index, or
    /// -1 for none.
    fn mark_as_answer() -> isize {
        calling_plugin().map_or(-1, |idx| isize::try_from(idx).unwrap_or(isize::MAX))
    }

    /// The router every route test hands to, answering with the mark it
    /// runs under. One for all of them, since the routers are the
    /// process's and the walk below gives every route this one. Nothing
    /// here asserts or panics: a panic cannot leave an `extern "C"`
    /// function, so it would abort the test binary.
    unsafe extern "C" fn mark_router(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize {
        if msg == NEST {
            let inner = plugin_route(wparam, mark_router);
            // SAFETY: a route to this router, which dereferences nothing.
            let seen = unsafe { inner(hwnd, 0, 0, lparam) };
            INNER.with(|c| c.set(usize::try_from(seen).ok()));
        }
        mark_as_answer()
    }

    /// A router other than [`mark_router`], answering something it never
    /// does.
    unsafe extern "C" fn other_router(_: Hwnd, _: u32, _: usize, _: isize) -> isize {
        -2
    }

    /// Send an empty message through `route`.
    fn send_through(route: HostDispatchFn) -> isize {
        // SAFETY: every router these tests use dereferences nothing.
        unsafe { route(std::ptr::null_mut(), 0, 0, 0) }
    }

    /// Every route marks its own plugin — each index listed once, in
    /// order — and the mark ends with the message.
    #[test]
    fn every_route_marks_its_own_plugin() {
        for idx in 0..MAX_ROUTED_PLUGINS {
            let seen = send_through(plugin_route(idx, mark_router));
            assert_eq!(
                usize::try_from(seen).ok(),
                Some(idx),
                "the route of plugin {idx} marked another"
            );
            assert_eq!(calling_plugin(), None, "and the mark ends with it");
        }
    }

    /// A route's mark is the innermost while its message is handled, inside
    /// a call the host is making and inside another plugin's message, and
    /// what was marked before comes back after it.
    #[test]
    fn routes_nest_inside_calls_and_each_other() {
        let outer = plugin_route(5, mark_router);
        let _calling = CallingPlugin::enter(40);
        // SAFETY: a route to `mark_router`, which dereferences nothing.
        let seen = unsafe { outer(std::ptr::null_mut(), NEST, 9, 0) };
        assert_eq!(
            INNER.with(Cell::get),
            Some(9),
            "the inner route did not mark its own plugin"
        );
        assert_eq!(
            seen, 5,
            "the outer route's mark did not come back after the inner one"
        );
        assert_eq!(
            calling_plugin(),
            Some(40),
            "the host's own mark did not come back"
        );
    }

    /// A plugin beyond the routes is marked no more than on Windows: not
    /// by its own message, not by a routed plugin's message being handled
    /// when it sends, and only by a call of the host's.
    #[test]
    fn a_plugin_beyond_the_routes_is_marked_only_by_the_hosts_calls() {
        let beyond = plugin_route(MAX_ROUTED_PLUGINS, mark_router);
        assert_eq!(
            send_through(beyond),
            -1,
            "a plugin beyond the routes was marked by its own message"
        );

        INNER.with(|c| c.set(Some(usize::MAX)));
        let routed = plugin_route(5, mark_router);
        // SAFETY: a route to `mark_router`, which dereferences nothing.
        unsafe { routed(std::ptr::null_mut(), NEST, MAX_ROUTED_PLUGINS, 0) };
        assert_eq!(
            INNER.with(Cell::get),
            None,
            "a plugin beyond the routes was taken for the routed plugin whose message was \
             being handled"
        );

        let _calling = CallingPlugin::enter(40);
        assert_eq!(
            send_through(beyond),
            40,
            "a plugin beyond the routes lost the mark of the host's call into it"
        );
    }

    /// A route that already hands to another router is not handed out for
    /// a different one: the router comes back unrouted rather than a route
    /// that would call the other.
    #[test]
    fn a_route_already_handing_elsewhere_is_not_handed_out() {
        // Claimed by the shared router first, as every route test does.
        let _ = plugin_route(17, mark_router);
        let given = plugin_route(17, other_router);
        assert_eq!(
            send_through(given),
            -2,
            "a route handing to another router was handed out"
        );
    }
}
