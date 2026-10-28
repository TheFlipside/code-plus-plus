//! Hosting the plugins: the window, the Scintilla views, the load and
//! the lifecycle. Win32 only.
//!
//! The host window is what `NppData._nppHandle` names, so every
//! `SendMessage(nppHandle, NPPM_*)` a plugin makes arrives at
//! [`host_proc`], which hands it to Code++'s own dispatcher
//! (`codepp_plugin_host::dispatch_nppm`) against the [`services`] the
//! harness provides. The two Scintilla views are real Scintilla
//! controls from the vendored, statically linked Scintilla, so a
//! plugin's `SCI_*` messages are answered by the editor Code++ ships.
//!
//! The load runs the way every Code++ backend runs it (DESIGN.md §6.4):
//! the three-phase `next_pending_load` / `execute_load` / `commit_load`
//! with no host borrow held while plugin code runs, then the load-time
//! notifications in Notepad++'s order once every plugin is in and its
//! menu exists. A plugin that queries the host from `setInfo` or
//! `beNotified` is therefore answered, as it is in the editor.

mod services;
mod watchdog;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::os::windows::fs::OpenOptionsExt as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use codepp_plugin_host::{
    dispatch_nppm, execute_load, is_plugin_cmd_id, LoadNotifications, Notification, NotifyTargets,
    NppData, PluginCommand, PluginHost,
};
use codepp_scintilla_sys::{
    Scintilla_RegisterClasses, SCI_GETLENGTH, SCI_GETTEXT, SCI_SELECTALL, SCI_SETCODEPAGE,
    SCI_SETTEXT, SC_CP_UTF8,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallWindowProcW, CreateMenu, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, GetMenuState, GetMessageW, GetWindowLongPtrW, MoveWindow,
    PeekMessageW, PostQuitMessage, RegisterClassExW, SendMessageW, SetMenu, SetWindowLongPtrW,
    ShowWindow, TranslateMessage, CW_USEDEFAULT, GWLP_WNDPROC, HMENU, MF_BYCOMMAND, MF_CHECKED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, PM_REMOVE, SW_SHOW, WINDOW_EX_STYLE, WM_CLOSE,
    WM_COMMAND, WM_QUIT, WM_SIZE, WNDCLASSEXW, WNDPROC, WS_CHILD, WS_CLIPCHILDREN, WS_HSCROLL,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE, WS_VSCROLL,
};

use crate::capture;
use crate::names::{is_host_message, SCI_END, SCI_FIRST};
use crate::options::Options;
use crate::report::{
    describe_shortcut, CommandReport, ItemReport, LoadOutcome, PluginReport, Report,
};
use crate::stage::{self, Staged};
use services::{Bridge, Recorder};
use watchdog::{AllowList, Watchdog};

pub use watchdog::TIMEOUT_EXIT_CODE;

/// The id of the one buffer the harness has open.
pub(crate) const BUFFER_ID: isize = 1;

/// How long the harness pumps messages after each step, so messages a
/// plugin posted (to itself, or `NPPM_MENUCOMMAND`'s `WM_COMMAND`) land
/// before the next step and before the report.
const SETTLE: Duration = Duration::from_millis(250);

/// How old a scratch folder from an earlier run must be before a new
/// run may remove it. A run's own folder cannot be removed while it
/// runs — the plugins in it stay mapped, and so locked, until the
/// process exits — so each run leaves its folder, and its lock, behind
/// and a later run sweeps it. [`LOCK_FILE`] is what keeps a sweep off a
/// live run; the age is a margin on top.
const STALE_SCRATCH: Duration = Duration::from_mins(10);

/// The prefix of every scratch folder.
const SCRATCH_PREFIX: &str = "npp-plugin-test-";

/// A file every run makes in its scratch folder and holds open, unshared,
/// until the process exits. A sweep deletes a folder only once it can
/// delete this file, i.e. once the run that made it has ended, however
/// long that run took.
const LOCK_FILE: &str = ".npp-plugin-test.lock";

/// A file `--keep` leaves in the scratch folder: a sweep never deletes a
/// folder carrying it, since the person who asked for it may still need
/// it.
const KEPT_FILE: &str = ".npp-plugin-test.kept";

/// The window class of the host window.
const HOST_CLASS: PCWSTR = w!("CodePlusPlusNppPluginTestHost");

/// The window class Scintilla registers.
const SCINTILLA_CLASS: PCWSTR = w!("Scintilla");

/// The handles and paths a run answers plugins with.
pub(crate) struct Env {
    /// `NppData._nppHandle`.
    pub npp: HWND,
    /// `NppData._scintillaMainHandle` and `_scintillaSecondHandle`.
    pub sci: [HWND; 2],
    /// The menu bar, for `NPPM_GETMENUHANDLE(NPPMAINMENU)`.
    pub main_menu: HMENU,
    /// The Plugins popup, for `NPPM_GETMENUHANDLE(NPPPLUGINMENU)`.
    pub plugin_menu: HMENU,
    /// The scratch root: `NPPM_GETNPPDIRECTORY` and
    /// `NPPM_GETNPPSETTINGSDIRPATH`.
    pub root: PathBuf,
    /// `<root>/plugins`: `NPPM_GETPLUGINHOMEPATH`.
    pub plugins_dir: PathBuf,
    /// `<root>/plugins/Config`: `NPPM_GETPLUGINSCONFIGDIR`.
    pub config_dir: PathBuf,
    /// The open buffer's file.
    pub buffer_path: PathBuf,
    /// The watchdog's list of windows a plugin registered — dock panels,
    /// modeless dialogs — which are not dialogs it is waiting on.
    /// `None` under `--interactive`, which runs no watchdog.
    pub allow: Option<AllowList>,
}

/// Everything a host message needs, behind one borrow.
pub(crate) struct Harness {
    pub host: PluginHost,
    pub env: Env,
    pub rec: Recorder,
}

/// Message counts, kept apart from [`Harness`] so they can be recorded
/// even when a message arrives while the harness is borrowed.
#[derive(Default)]
struct MessageLog {
    host: BTreeMap<u32, u32>,
    declined: BTreeMap<u32, u32>,
    sci: BTreeMap<u32, u32>,
}

thread_local! {
    static STATE: RefCell<Option<Harness>> = const { RefCell::new(None) };
    static LOG: RefCell<MessageLog> = RefCell::new(MessageLog::default());
    /// Scintilla's own window procedure, under the counting subclass.
    static SCI_PROC: Cell<isize> = const { Cell::new(0) };
    /// The main Scintilla view, for laying it out on `WM_SIZE`.
    static SCI_MAIN: Cell<isize> = const { Cell::new(0) };
    /// Set while the harness itself messages Scintilla, so its own
    /// messages are not counted as a plugin's.
    static HARNESS_SENDING: Cell<bool> = const { Cell::new(false) };
    /// `--interactive`: the window is the user's to close.
    static INTERACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Borrow the harness for `f`, or `None` when it is already borrowed —
/// which happens only when plugin code the harness called while
/// borrowed calls back in (a host message never arrives before the
/// harness is set up: no plugin is loaded until then).
pub(crate) fn with_state<R>(f: impl FnOnce(&mut Harness) -> R) -> Option<R> {
    STATE.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

/// What [`run`] hands back: the report, and the teardown still to come
/// once the report body is out.
///
/// Call [`Run::finish`] before letting it go. Dropped without it — a
/// panic unwinding past it, say — the lock closes and the `TempDir` then
/// fails on the mapped plugins, leaving a folder no sweep recognises.
pub struct Run {
    /// What the run found.
    pub report: Report,
    npp: Option<HWND>,
    watchdog: Option<Watchdog>,
    /// How many of the watchdog's dialogs the report already holds.
    dialogs_reported: usize,
    lock: Option<File>,
    scratch: Option<tempfile::TempDir>,
}

impl Run {
    /// Destroy the host window, as the editor's own teardown does. The
    /// watchdog still runs, so a plugin that
    /// opens a dialog or hangs while its windows are destroyed is caught
    /// as during any other step; a dialog it opens is added to the report
    /// as a failure.
    ///
    /// Separate from [`run`] so the CLI can print the report body first:
    /// a plugin that crashes here still leaves what the run found behind,
    /// and the crash shows in the exit status. The verdict line is printed
    /// after this, from [`Report::render_tail`].
    pub fn finish(&mut self) {
        if let Some(watchdog) = &self.watchdog {
            watchdog.phase("teardown".to_string(), false);
        }
        if let Some(npp) = self.npp.take() {
            // SAFETY: `npp` is the host window this thread created and
            // has not destroyed; its plugins' libraries stay mapped
            // (they are never unmapped), so the `WM_DESTROY` their
            // child windows receive runs mapped code.
            unsafe {
                let _ = DestroyWindow(npp);
            }
            settle(SETTLE);
        }
        if let Some(watchdog) = self.watchdog.take() {
            let late = watchdog.stop().split_off(self.dialogs_reported);
            self.report.add_late_dialogs(late);
        }
        // The scratch folder stays, lock and all. The staged libraries are
        // mapped, and so locked, until the process exits; removing what
        // can be removed now would take the lock file with it and leave a
        // folder no sweep recognises. The process's exit closes the lock,
        // and a later run's sweep then removes the whole folder.
        if let Some(scratch) = self.scratch.take() {
            let _ = scratch.keep();
        }
    }
}

/// The scratch folder's layout.
///
/// `lock` is declared before `dir` so that dropping a `Scratch` — on a
/// run that ends before any plugin is mapped — closes the lock first and
/// the `TempDir` can then remove everything.
struct Scratch {
    lock: File,
    /// `None` for a folder kept with `--keep`, which nothing may remove.
    dir: Option<tempfile::TempDir>,
    root: PathBuf,
    plugins_dir: PathBuf,
    config_dir: PathBuf,
    buffer_path: PathBuf,
}

impl Scratch {
    /// Make the folder, take its lock, mark it kept when `keep`, and
    /// write the buffer's file into it. The kept mark goes in first, so a
    /// run cut short — a crash, a timeout — still leaves a folder no sweep
    /// removes.
    fn prepare(text: &str, keep: bool) -> Result<Self, String> {
        Self::prepare_in(&std::env::temp_dir(), text, keep)
    }

    /// [`Self::prepare`] under `temp`.
    fn prepare_in(temp: &Path, text: &str, keep: bool) -> Result<Self, String> {
        let dir = tempfile::Builder::new()
            .prefix(SCRATCH_PREFIX)
            .tempdir_in(temp)
            .map_err(|e| format!("could not make a scratch folder: {e}"))?;
        let root = dir.path().to_path_buf();
        // Opened with no sharing: no other process can open, and so
        // delete, it while this one holds it.
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(root.join(LOCK_FILE))
            .map_err(|e| format!("could not lock {}: {e}", root.display()))?;
        if keep {
            std::fs::write(root.join(KEPT_FILE), b"kept by --keep\n")
                .map_err(|e| format!("could not mark {} kept: {e}", root.display()))?;
        }
        let plugins_dir = root.join("plugins");
        let config_dir = plugins_dir.join("Config");
        let docs = root.join("docs");
        let buffer_path = docs.join("harness.txt");
        std::fs::create_dir_all(&config_dir)
            .and_then(|()| std::fs::create_dir_all(&docs))
            .and_then(|()| std::fs::write(&buffer_path, text.as_bytes()))
            .map_err(|e| format!("could not prepare {}: {e}", root.display()))?;
        // A kept folder leaves the `TempDir`'s hands now, so no drop on any
        // path — an early failure included — can remove it.
        let dir = if keep {
            let _ = dir.keep();
            None
        } else {
            Some(dir)
        };
        Ok(Self {
            lock,
            dir,
            root,
            plugins_dir,
            config_dir,
            buffer_path,
        })
    }
}

/// Tells the watchdog, when there is one, which step the run is in.
struct Steps<'a>(Option<&'a Watchdog>);

impl Steps<'_> {
    fn enter(&self, name: String, command: bool) {
        if let Some(watchdog) = self.0 {
            watchdog.phase(name, command);
        }
    }
}

/// Run the harness: stage, host, load, notify, run the commands, shut
/// down, and report. The teardown is [`Run::finish`].
#[must_use]
pub fn run(options: &Options) -> Run {
    capture::install(options.verbose);
    sweep_stale_scratch();
    INTERACTIVE.set(options.interactive);
    let mut outcome = Run {
        report: Report::default(),
        npp: None,
        watchdog: None,
        dialogs_reported: 0,
        lock: None,
        scratch: None,
    };
    let scratch = match Scratch::prepare(&options.text, options.keep) {
        Ok(scratch) => scratch,
        Err(e) => {
            outcome.report.fail(e);
            return outcome;
        }
    };
    let mut report = Report::default();
    let staged = stage_all(options, &scratch.plugins_dir, &mut report);

    // An early return from here until the load drops `scratch`, which
    // removes the folder whole: nothing in it is mapped yet. An early
    // return added after the load must hand the folder and its lock to the
    // `Run` instead, as the end of this function does, or the drop would
    // remove the lock file but not the mapped plugins and leave a folder
    // no sweep recognises.
    // SAFETY: plain Win32 window construction on this thread.
    let Some((npp, sci, main_menu, plugin_menu)) = (unsafe { create_host_window() }) else {
        report.fail("could not create the host window or its Scintilla views");
        outcome.report = report;
        return outcome;
    };
    outcome.npp = Some(npp);
    set_buffer_text(sci[0], &options.text);

    let mut host = PluginHost::new();
    // Always `Ok`: a folder discovery cannot read is "no plugins", which
    // the report then shows as each plugin not discovered.
    let _ = host.discover(&scratch.plugins_dir);

    let watchdog = if options.interactive {
        None
    } else {
        // SAFETY: no preconditions.
        let ui_thread = unsafe { GetCurrentThreadId() };
        let Some(watchdog) = Watchdog::start(ui_thread, npp.0 as isize, options.timeout) else {
            // Without it nothing bounds a step or answers a dialog, and an
            // unattended run could hang.
            report.fail("could not start the watchdog thread");
            outcome.report = report;
            return outcome;
        };
        Some(watchdog)
    };
    let steps = Steps(watchdog.as_ref());

    STATE.with(|cell| {
        *cell.borrow_mut() = Some(Harness {
            host,
            env: Env {
                npp,
                sci,
                main_menu,
                plugin_menu,
                root: scratch.root.clone(),
                plugins_dir: scratch.plugins_dir.clone(),
                config_dir: scratch.config_dir.clone(),
                buffer_path: scratch.buffer_path.clone(),
                allow: watchdog.as_ref().map(Watchdog::allow_list),
            },
            rec: Recorder::default(),
        });
    });

    let npp_data = NppData {
        npp_handle: npp.0,
        scintilla_main_handle: sci[0].0,
        scintilla_second_handle: sci[1].0,
    };
    let notices = load_pass(npp_data, &steps, &mut report);
    let _ = with_state(install_plugin_menus);

    steps.enter(
        "NPPN_TBMODIFICATION, NPPN_BUFFERACTIVATED, NPPN_READY".to_string(),
        false,
    );
    notices.deliver(npp.0, || usize::try_from(BUFFER_ID).ok(), || {});
    settle(SETTLE);

    run_commands(options, &staged, sci[0], &steps, &mut report);
    if options.interactive {
        interact(npp);
    }

    steps.enter("NPPN_BEFORESHUTDOWN, NPPN_SHUTDOWN".to_string(), false);
    let targets = with_state(|h| NotifyTargets::snapshot(&h.host)).unwrap_or_default();
    targets.deliver(&Notification::BeforeShutdown, npp.0);
    targets.deliver(&Notification::Shutdown, npp.0);
    settle(SETTLE);

    steps.enter("collecting the report".to_string(), false);
    if let Some(watchdog) = &watchdog {
        report.dialogs = watchdog.dialogs_so_far();
        outcome.dialogs_reported = report.dialogs.len();
    }
    collect(&mut report, &staged);
    report.judge(options.strict);
    if options.keep {
        report.scratch = Some(scratch.root.clone());
    }
    // Held, with the folder, until the process exits: see `Run::finish`.
    outcome.lock = Some(scratch.lock);
    outcome.scratch = scratch.dir;
    outcome.watchdog = watchdog;
    outcome.report = report;
    outcome
}

/// Load every discovered plugin the way the editor does: take the load
/// under a borrow, run the plugin's entry points with none held, commit
/// under a borrow. Hands back the load-time notifications the plugins
/// are owed.
fn load_pass(npp_data: NppData, steps: &Steps<'_>, report: &mut Report) -> LoadNotifications {
    let mut notices = LoadNotifications::default();
    while let Some(pending) = with_state(|h| h.host.next_pending_load()).flatten() {
        let file = with_state(|h| {
            h.host
                .iter()
                .nth(pending.idx())
                .map(codepp_plugin_host::PluginInfo::filename)
        })
        .flatten()
        .unwrap_or_default();
        steps.enter(format!("load {file}"), false);
        let loaded = catch_unwind(AssertUnwindSafe(|| execute_load(&pending, npp_data, None)))
            .unwrap_or_else(|_| Err("the host panicked while loading the plugin".to_string()));
        match with_state(|h| h.host.commit_load(&pending, loaded)) {
            Some(Ok(Some(ready))) => notices.push(ready),
            Some(Ok(None) | Err(_)) => {}
            None => {
                // Nothing holds the harness here, so this cannot happen;
                // were it to, the plugins after this one would never load.
                report.fail(format!(
                    "the harness was busy when {file}'s load came to be committed"
                ));
                break;
            }
        }
    }
    notices
}

/// Run each `--run` command, in order, with nothing borrowed.
fn run_commands(
    options: &Options,
    staged: &[Staged],
    main_view: HWND,
    steps: &Steps<'_>,
    report: &mut Report,
) {
    for spec in &options.run {
        let resolved = with_state(|h| resolve_command(h, spec, staged)).flatten();
        report.commands.push(CommandReport {
            spec: spec.clone(),
            resolved: resolved
                .as_ref()
                .map(|found| (found.label.clone(), found.cmd_id)),
            matches: resolved.as_ref().map_or(0, |found| found.matches),
        });
        let Some(found) = resolved else {
            continue;
        };
        // Before each command, so a chain — encode, then decode — works
        // on the whole buffer each time.
        if options.select_all {
            sci_send(main_view, SCI_SELECTALL, 0, 0);
        }
        steps.enter(format!("command {:?}", found.label), true);
        // SAFETY: on the UI thread, with the host alive and nothing
        // borrowed; the plugin's library stays mapped.
        unsafe { found.command.run() };
        settle(SETTLE);
    }
}

/// Stage every plugin, recording the ones that could not be.
fn stage_all(options: &Options, plugins_dir: &Path, report: &mut Report) -> Vec<Staged> {
    let mut staged = Vec::new();
    for dll in &options.plugins {
        match stage::stage(plugins_dir, dll) {
            Ok(s) => staged.push(s),
            Err(e) => report.plugins.push(PluginReport {
                source: dll.clone(),
                file: dll
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                outcome: LoadOutcome::Failed(format!("could not stage it: {e}")),
                name: None,
                items: Vec::new(),
            }),
        }
    }
    staged
}

/// Register the classes and create the host window, its menus and the
/// two Scintilla views. On any failure the window made so far is
/// destroyed.
///
/// # Safety
///
/// Call on the thread that will pump the window's messages.
unsafe fn create_host_window() -> Option<(HWND, [HWND; 2], HMENU, HMENU)> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None).ok()?.into();
        if Scintilla_RegisterClasses(instance.0) == 0 {
            return None;
        }
        let class = WNDCLASSEXW {
            cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).ok()?,
            lpfnWndProc: Some(host_proc),
            hInstance: instance,
            lpszClassName: HOST_CLASS,
            ..Default::default()
        };
        // A second registration in the same process fails harmlessly.
        let _ = RegisterClassExW(&raw const class);
        let npp = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            HOST_CLASS,
            w!("Code++ plugin test host"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            900,
            600,
            None,
            None,
            Some(instance),
            None,
        )
        .ok()?;
        let built = build_host_contents(npp, instance);
        if built.is_none() {
            let _ = DestroyWindow(npp);
        }
        built.map(|(sci, main_menu, plugin_menu)| (npp, sci, main_menu, plugin_menu))
    }
}

/// The menus and Scintilla views of the host window `npp`.
///
/// # Safety
///
/// As [`create_host_window`]; `npp` is the window it just made.
unsafe fn build_host_contents(npp: HWND, instance: HINSTANCE) -> Option<([HWND; 2], HMENU, HMENU)> {
    unsafe {
        let main_menu = CreateMenu().ok()?;
        let plugin_menu = CreatePopupMenu().ok()?;
        AppendMenuW(main_menu, MF_POPUP, plugin_menu.0 as usize, w!("&Plugins")).ok()?;
        SetMenu(npp, Some(main_menu)).ok()?;

        let mut sci = [HWND::default(); 2];
        for (i, view) in sci.iter_mut().enumerate() {
            let style = if i == 0 {
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_HSCROLL
            } else {
                WS_CHILD | WS_VSCROLL | WS_HSCROLL
            };
            *view = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                SCINTILLA_CLASS,
                PCWSTR::null(),
                style,
                0,
                0,
                880,
                540,
                Some(npp),
                None,
                Some(instance),
                None,
            )
            .ok()?;
            SendMessageW(
                *view,
                SCI_SETCODEPAGE,
                Some(WPARAM(SC_CP_UTF8 as usize)),
                None,
            );
            // Count the Scintilla messages plugins send. Both views are
            // the same class, so one saved procedure serves both; it is
            // saved before the subclass is installed, so the subclass
            // never runs without it.
            let original = GetWindowLongPtrW(*view, GWLP_WNDPROC);
            if original == 0 {
                return None;
            }
            SCI_PROC.set(original);
            let subclass: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
                sci_proc;
            if SetWindowLongPtrW(*view, GWLP_WNDPROC, subclass as *const () as isize) == 0 {
                return None;
            }
        }
        SCI_MAIN.set(sci[0].0 as isize);
        Some((sci, main_menu, plugin_menu))
    }
}

/// The host window's procedure: every host message to the dispatcher,
/// a plugin's menu command to the plugin.
unsafe extern "system" fn host_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = catch_unwind(AssertUnwindSafe(|| on_host_message(msg, wparam, lparam)));
    match handled {
        Ok(Some(result)) => LRESULT(result),
        // SAFETY: the arguments are the ones this procedure was given.
        Ok(None) => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        Err(_) => {
            tracing::error!(msg, "the harness panicked handling a host message");
            // The default answer, not 0, which for some messages
            // (`WM_NCCREATE`, `WM_CREATE`) would abort the window.
            // SAFETY: as above.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

fn on_host_message(msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
    if is_host_message(msg) {
        LOG.with_borrow_mut(|log| *log.host.entry(msg).or_insert(0) += 1);
        let answered = with_state(|h| {
            let mut bridge = Bridge(h);
            // SAFETY: the plugin's pointers are its own to keep valid
            // for the call, as the ABI requires; the dispatcher bounds
            // every write it makes through them.
            unsafe { dispatch_nppm(&mut bridge, msg, wparam.0, lparam.0) }
        });
        if let Some(result) = answered {
            return Some(result.unwrap_or(0));
        }
        // Re-entered from plugin code the harness called while borrowed
        // — the editor declines the same way.
        LOG.with_borrow_mut(|log| *log.declined.entry(msg).or_insert(0) += 1);
        return Some(0);
    }
    match msg {
        WM_COMMAND => {
            let id = i32::try_from(wparam.0 & 0xFFFF).unwrap_or(0);
            if !is_plugin_cmd_id(id) {
                return None;
            }
            let command: Option<PluginCommand> =
                with_state(|h| h.host.lookup_command(id)).flatten();
            if let Some(command) = command {
                // SAFETY: on the UI thread, nothing borrowed, the host
                // alive; the plugin's library stays mapped.
                unsafe { command.run() };
            }
            Some(0)
        }
        WM_SIZE => {
            let main = SCI_MAIN.get();
            if main != 0 {
                let width = i32::try_from(lparam.0 & 0xFFFF).unwrap_or(0);
                let height = i32::try_from((lparam.0 >> 16) & 0xFFFF).unwrap_or(0);
                // SAFETY: `main` is the view this thread created.
                unsafe {
                    let _ = MoveWindow(
                        HWND(main as *mut core::ffi::c_void),
                        0,
                        0,
                        width,
                        height,
                        true,
                    );
                }
            }
            Some(0)
        }
        WM_CLOSE => {
            if INTERACTIVE.get() {
                // The user closed the window: end the interactive loop.
                // The run goes on to shutdown with the window still up, as
                // the editor's quit does.
                // SAFETY: no preconditions.
                unsafe { PostQuitMessage(0) };
            } else {
                // A plugin asked the hidden host to close. Recorded; the
                // run keeps to its own steps.
                let _ = with_state(|h| h.rec.request_close());
            }
            Some(0)
        }
        _ => None,
    }
}

/// The counting subclass of both Scintilla views.
unsafe extern "system" fn sci_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if (SCI_FIRST..SCI_END).contains(&msg) && !HARNESS_SENDING.get() {
        let _ = LOG.try_with(|log| {
            if let Ok(mut log) = log.try_borrow_mut() {
                *log.sci.entry(msg).or_insert(0) += 1;
            }
        });
    }
    // SAFETY: `SCI_PROC` holds the views' own procedure, read with
    // `GetWindowLongPtrW` before the subclass was installed and checked
    // non-null: a `WNDPROC` stored as an integer, and `Option<fn>` has
    // the same size and representation.
    let original: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(SCI_PROC.get()) };
    // SAFETY: forwarding this procedure's own arguments to the
    // procedure it replaced.
    unsafe { CallWindowProcW(original, hwnd, msg, wparam, lparam) }
}

/// Send a Scintilla message on the harness's own behalf, so it is not
/// counted as a plugin's.
pub(crate) fn sci_send(view: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    let was = HARNESS_SENDING.replace(true);
    // SAFETY: `view` is one of this thread's Scintilla views, and any
    // pointer in `lparam` is the caller's, live for the call.
    let result = unsafe { SendMessageW(view, msg, Some(WPARAM(wparam)), Some(LPARAM(lparam))) };
    HARNESS_SENDING.set(was);
    result.0
}

/// Replace a view's text.
pub(crate) fn set_buffer_text(view: HWND, text: &str) {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    sci_send(view, SCI_SETTEXT, 0, bytes.as_ptr() as isize);
}

/// A view's text.
pub(crate) fn buffer_text(view: HWND) -> String {
    let len = usize::try_from(sci_send(view, SCI_GETLENGTH, 0, 0)).unwrap_or(0);
    let mut bytes = vec![0u8; len + 1];
    sci_send(view, SCI_GETTEXT, len + 1, bytes.as_mut_ptr() as isize);
    bytes.truncate(len);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Pump this thread's messages for `duration`, and no longer: a plugin
/// that keeps posting to itself does not hold the step open.
fn settle(duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut msg = MSG::default();
    while Instant::now() < deadline {
        // SAFETY: standard message pump on the thread that owns the
        // windows.
        let got = unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool();
        if !got {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        if msg.message == WM_QUIT {
            // Put it back for `interact`, whose loop it is meant to end.
            // SAFETY: no preconditions.
            unsafe { PostQuitMessage(i32::try_from(msg.wParam.0).unwrap_or(0)) };
            return;
        }
        // SAFETY: as above.
        unsafe {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

/// Show the window and pump until it is closed.
fn interact(npp: HWND) {
    // SAFETY: standard message pump on the thread that owns `npp`.
    unsafe {
        let _ = ShowWindow(npp, SW_SHOW);
        let mut msg = MSG::default();
        // `GetMessageW` answers -1 on an error, which must end the loop
        // as `WM_QUIT`'s 0 does.
        while GetMessageW(&raw mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

/// Give each loaded plugin its submenu, as the editor does between the
/// load and the load-time notifications, so a plugin that ticks its own
/// item from `NPPN_TBMODIFICATION` or `NPPN_READY` finds it there.
fn install_plugin_menus(h: &mut Harness) {
    for plugin in h.host.iter() {
        let Some(items) = plugin.func_items() else {
            continue;
        };
        // SAFETY: menu construction on the thread that owns the menus.
        unsafe {
            let Ok(popup) = CreatePopupMenu() else {
                continue;
            };
            for item in items {
                if item.is_command() {
                    let mut flags = MF_STRING;
                    if item.init2_check != 0 {
                        flags |= MF_CHECKED;
                    }
                    let label = wide(&item_label(&item.item_name));
                    let _ = AppendMenuW(
                        popup,
                        flags,
                        usize::try_from(item.cmd_id).unwrap_or(0),
                        PCWSTR(label.as_ptr()),
                    );
                } else {
                    let _ = AppendMenuW(popup, MF_SEPARATOR, 0, PCWSTR::null());
                }
            }
            let name = wide(&plugin.display_label());
            let _ = AppendMenuW(
                h.env.plugin_menu,
                MF_POPUP,
                popup.0 as usize,
                PCWSTR(name.as_ptr()),
            );
        }
    }
}

/// A command a `--run` spec resolved to.
struct Found {
    label: String,
    cmd_id: i32,
    command: PluginCommand,
    /// How many items the spec matched; the first is run.
    matches: usize,
}

/// Find the command `spec` names: a label, with or without its `&`
/// mnemonic, across the loaded plugins in load order, or `#N` for the
/// first staged plugin's Nth item.
fn resolve_command(h: &Harness, spec: &str, staged: &[Staged]) -> Option<Found> {
    let (item, matches) = if let Some(index) = spec.strip_prefix('#') {
        let index: usize = index.parse().ok()?;
        let first = staged.first()?;
        let plugin = h.host.iter().find(|p| p.module_key() == first.module_key)?;
        (*plugin.func_items()?.get(index)?, 1)
    } else {
        let mut found = h
            .host
            .iter()
            .filter_map(|p| p.func_items())
            .flatten()
            .filter(|f| {
                let label = item_label(&f.item_name);
                label == spec || label.replace('&', "") == spec
            });
        let first = *found.next()?;
        (first, 1 + found.count())
    };
    let command = h.host.lookup_command(item.cmd_id)?;
    Some(Found {
        label: item_label(&item.item_name),
        cmd_id: item.cmd_id,
        command,
        matches,
    })
}

/// Gather everything recorded into the report.
fn collect(report: &mut Report, staged: &[Staged]) {
    let log = LOG.take();
    report.nppm = log.host.into_iter().collect();
    report.declined = log.declined.into_iter().collect();
    report.sci = log.sci.into_iter().collect();

    let mut unhandled = Vec::new();
    let mut warnings: Vec<(String, u32)> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for event in capture::take() {
        if let Some(msg) = event.unhandled_message() {
            unhandled.push(msg);
            continue;
        }
        let text = event.text();
        if let Some(&at) = seen.get(&text) {
            warnings[at].1 += 1;
        } else {
            seen.insert(text.clone(), warnings.len());
            warnings.push((text, 1));
        }
    }
    report.unhandled = crate::report::tally(unhandled);
    report.host_warnings = warnings;

    let _ = with_state(|h| {
        for s in staged {
            report.plugins.push(plugin_report(h, s));
        }
        h.rec.fill(report);
        report.buffer = Some(buffer_text(h.env.sci[0]));
    });
}

fn plugin_report(h: &Harness, staged: &Staged) -> PluginReport {
    let mut out = PluginReport {
        source: staged.source.clone(),
        file: staged.file.clone(),
        outcome: LoadOutcome::NotDiscovered,
        name: None,
        items: Vec::new(),
    };
    let Some(plugin) = h.host.iter().find(|p| p.module_key() == staged.module_key) else {
        return out;
    };
    out.outcome = if plugin.is_loaded() {
        LoadOutcome::Loaded
    } else if let Some(why) = plugin.failed_reason() {
        LoadOutcome::Failed(why.to_string())
    } else {
        LoadOutcome::NotLoaded
    };
    if !plugin.is_loaded() {
        return out;
    }
    out.name = Some(plugin.display_label());
    let shortcuts = plugin.shortcut_defaults().unwrap_or_default();
    for (i, item) in plugin.func_items().unwrap_or_default().iter().enumerate() {
        // SAFETY: `GetMenuState` only reads the menu this thread owns.
        let state = unsafe {
            GetMenuState(
                h.env.main_menu,
                u32::try_from(item.cmd_id).unwrap_or(0),
                MF_BYCOMMAND,
            )
        };
        out.items.push(ItemReport {
            label: item_label(&item.item_name),
            cmd_id: item.cmd_id,
            separator: !item.is_command(),
            checked: state != u32::MAX && state & MF_CHECKED.0 != 0,
            shortcut: shortcuts
                .get(i)
                .copied()
                .flatten()
                .map(|k| describe_shortcut(k.is_ctrl != 0, k.is_alt != 0, k.is_shift != 0, k.key)),
        });
    }
    out
}

/// A `FuncItem` label, up to its first NUL.
fn item_label(name: &[u16]) -> String {
    let end = name.iter().position(|&u| u == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

/// `text` as a NUL-terminated UTF-16 string.
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Remove the scratch folders earlier runs left behind. Best effort.
///
/// A folder goes only when all of these hold: its name has our prefix, it
/// is a real folder rather than a link, it is older than
/// [`STALE_SCRATCH`], it does not carry [`KEPT_FILE`], and its
/// [`LOCK_FILE`] can be deleted — which only happens once the run that
/// made it, however long it ran, has exited. A folder with no lock file
/// is left alone: it is not one this harness made, or its run never got
/// as far as taking the lock.
fn sweep_stale_scratch() {
    sweep_scratch_in(&std::env::temp_dir(), STALE_SCRATCH);
}

/// [`sweep_stale_scratch`] over `temp`, taking folders older than
/// `older_than`.
fn sweep_scratch_in(temp: &Path, older_than: Duration) {
    let Ok(entries) = std::fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        let is_ours = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(SCRATCH_PREFIX));
        // `DirEntry::file_type` does not follow a link, so a link someone
        // planted under our prefix is never walked.
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !is_ours || !is_dir {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > older_than);
        let folder = entry.path();
        if !stale || folder.join(KEPT_FILE).exists() {
            continue;
        }
        if std::fs::remove_file(folder.join(LOCK_FILE)).is_ok() {
            // Best effort: should something else hold a file in it (a
            // virus scanner, say), what is left has no lock and stays.
            let _ = std::fs::remove_dir_all(&folder);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder named as a run names one, under `temp`.
    fn scratch_in(temp: &Path, name: &str) -> PathBuf {
        let folder = temp.join(format!("{SCRATCH_PREFIX}{name}"));
        std::fs::create_dir_all(folder.join("plugins")).expect("mkdir");
        std::fs::write(folder.join("plugins").join("x.txt"), b"x").expect("write");
        folder
    }

    fn lock(folder: &Path) -> File {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(folder.join(LOCK_FILE))
            .expect("lock")
    }

    #[test]
    fn the_sweep_takes_only_ended_unkept_runs_of_ours() {
        let temp = tempfile::tempdir().expect("tempdir");
        // A run that has ended: its lock is there but nobody holds it.
        let ended = scratch_in(temp.path(), "ended");
        drop(lock(&ended));
        // A run still going, however long it has run: its lock is held.
        let live = scratch_in(temp.path(), "live");
        let held = lock(&live);
        // A run kept by `--keep`.
        let kept = scratch_in(temp.path(), "kept");
        drop(lock(&kept));
        std::fs::write(kept.join(KEPT_FILE), b"").expect("write");
        // A folder with no lock: not ours, or never got that far.
        let unlocked = scratch_in(temp.path(), "unlocked");
        // Not ours at all.
        let other = temp.path().join("someone-else");
        std::fs::create_dir(&other).expect("mkdir");
        std::fs::write(other.join(LOCK_FILE), b"").expect("write");

        sweep_scratch_in(temp.path(), Duration::ZERO);

        assert!(!ended.exists(), "an ended run's folder is swept");
        assert!(
            live.join("plugins").join("x.txt").exists(),
            "a live run is untouched"
        );
        assert!(kept.exists(), "a kept folder is untouched");
        assert!(unlocked.exists(), "a folder without a lock is untouched");
        assert!(other.exists(), "a folder without our prefix is untouched");
        drop(held);
    }

    /// A `Run` holding `scratch`, as `run` hands one back, with no window
    /// or watchdog.
    fn run_holding(scratch: Scratch) -> Run {
        Run {
            report: Report::default(),
            npp: None,
            watchdog: None,
            dialogs_reported: 0,
            lock: Some(scratch.lock),
            scratch: scratch.dir,
        }
    }

    #[test]
    fn a_finished_run_leaves_its_folder_for_a_later_sweep() {
        let temp = tempfile::tempdir().expect("tempdir");
        let scratch = Scratch::prepare_in(temp.path(), "text", false).expect("prepare");
        let root = scratch.root.clone();
        // Stands in for a staged plugin, mapped and so locked until the
        // process exits.
        let dll = root.join("plugins").join("x.dll");
        std::fs::write(&dll, b"MZ").expect("write");
        let mapped = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&dll)
            .expect("hold the file");

        let mut run = run_holding(scratch);
        run.finish();
        // The whole folder stays, lock file included: removing what could
        // be removed would take the lock and orphan the rest.
        assert!(root.join(LOCK_FILE).exists(), "the lock file is kept");
        assert!(root.join("docs").join("harness.txt").exists());
        // While the run's process lives, no sweep touches it.
        sweep_scratch_in(temp.path(), Duration::ZERO);
        assert!(root.join(LOCK_FILE).exists(), "a live run is not swept");

        // The process exits: its handles close.
        drop(run);
        drop(mapped);
        sweep_scratch_in(temp.path(), Duration::ZERO);
        assert!(!root.exists(), "an ended run's folder is swept whole");
    }

    #[test]
    fn a_kept_folder_is_marked_before_anything_can_cut_the_run_short() {
        let temp = tempfile::tempdir().expect("tempdir");
        let scratch = Scratch::prepare_in(temp.path(), "text", true).expect("prepare");
        let root = scratch.root.clone();
        assert!(root.join(KEPT_FILE).exists(), "marked at prepare time");
        // A run that dies at once: its lock closes, its folder stays.
        drop(run_holding(scratch));
        sweep_scratch_in(temp.path(), Duration::ZERO);
        assert!(root.exists(), "a kept folder is never swept");
    }

    #[test]
    fn a_run_that_ends_before_any_plugin_leaves_nothing() {
        // Dropping a `Scratch` closes the lock before the `TempDir`
        // removes the folder, so nothing is left behind.
        let temp = tempfile::tempdir().expect("tempdir");
        let scratch = Scratch::prepare_in(temp.path(), "text", false).expect("prepare");
        let root = scratch.root.clone();
        drop(scratch);
        assert!(!root.exists());
    }

    #[test]
    fn the_sweep_waits_for_a_folder_to_age() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ended = scratch_in(temp.path(), "young");
        drop(lock(&ended));
        sweep_scratch_in(temp.path(), STALE_SCRATCH);
        assert!(
            ended.exists(),
            "a folder younger than the threshold is untouched"
        );
    }
}
