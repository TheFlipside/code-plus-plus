//! The host a plugin talks to: one buffer in one view, real menus and
//! real Scintilla, and a record of everything the plugin asks for.
//!
//! The dispatcher (`dispatch_nppm`) decodes every message and does the
//! pointer work; what it asks of the host goes through these methods.
//! Where a request has a real answer the harness can give without
//! leaving its scratch folder — the buffer's text, its path, a command
//! id, a menu check, a Scintilla view — it gives it. Where the answer
//! would mean acting on the user's machine — opening another file,
//! saving elsewhere, the real clipboard, a dialog — it records the
//! request and answers as a host that did nothing would. Built-in menu
//! commands, the dirty flag, font smoothing and the border edge are
//! recorded and answered as done but not applied: the harness has no
//! editor chrome for them to act on. Where the editor refuses a request
//! — a language number no plugin is given, a line-number mode it does
//! not know — the harness refuses it too, so a plugin is never told
//! "done" for something the editor would have turned down.
//!
//! Some of these run plugin code while the dispatcher holds the
//! harness: `forward_plugin_message` calls another plugin's
//! `messageProc`, and making a Scintilla view sends its parent
//! `WM_PARENTNOTIFY`. A message that plugin code sends back finds the
//! harness's `RefCell` borrowed and is declined (`super::with_state`
//! answers `None`), as the editor declines it — there is no second
//! `&mut Harness`, only the refusal.

use std::cell::RefCell;
use std::path::PathBuf;

use codepp_core::LangType;
use codepp_plugin_host::dispatch::L_USER;
use codepp_plugin_host::{
    docking::answers_hwnd_lookup, is_plugin_cmd_id, read_dock_disp_info, DockDialogParams,
    HostServices, Hwnd, NppDarkModeColors, ShortcutKey, LINENUMWIDTH_CONSTANT,
    LINENUMWIDTH_DYNAMIC, MAX_PLUGIN_SCINTILLAS, NPPMAINMENU, NPPPLUGINMENU, UNI_COOKIE,
    WIN_FORMAT,
};
use codepp_scintilla_sys::{
    SCI_GETZOOM, SCI_SETCODEPAGE, SCI_SETSAVEPOINT, SCI_STYLEGETBACK, SCI_STYLEGETFORE, SC_CP_UTF8,
    STYLE_DEFAULT,
};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CheckMenuItem, CreateWindowExW, IsWindow, PostMessageW, SendMessageW, MF_BYCOMMAND, MF_CHECKED,
    MF_UNCHECKED, WINDOW_EX_STYLE, WM_COMMAND, WS_CHILD, WS_CLIPCHILDREN, WS_HSCROLL, WS_VSCROLL,
};

use super::{buffer_text, set_buffer_text, Harness, BUFFER_ID};
use crate::report::{DockReport, Report};

/// The most requests, status texts and clipboard offers kept; a plugin
/// asking in a loop must not grow the harness without bound. The report
/// says how many were dropped.
const MAX_REQUESTS: usize = 1000;

/// The most dock panels, modeless dialogs and toolbar buttons the
/// harness takes, each: the editor's cap on dock panels, applied to the
/// other two as a bound of the same size.
const MAX_REGISTRATIONS: usize = 64;

/// A list that stops growing at its cap and counts what it turned away.
struct Capped<T> {
    items: Vec<T>,
    dropped: usize,
    cap: usize,
}

impl<T> Capped<T> {
    const fn new(cap: usize) -> Self {
        Self {
            items: Vec::new(),
            dropped: 0,
            cap,
        }
    }

    fn push(&mut self, item: T) {
        if self.items.len() < self.cap {
            self.items.push(item);
        } else {
            self.dropped += 1;
        }
    }

    /// The items, with how many were turned away recorded under `list`.
    fn take(&mut self, list: &'static str, dropped: &mut Vec<(&'static str, usize)>) -> Vec<T> {
        if self.dropped > 0 {
            dropped.push((list, self.dropped));
        }
        self.dropped = 0;
        std::mem::take(&mut self.items)
    }
}

/// What plugins asked the host for, and the state they changed.
///
/// The `bool`s are the host's independent show/hide toggles, each set
/// and read back by its own message pair, so clippy's suggestion to
/// fold them into a state machine does not apply.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Recorder {
    /// Requests the harness recorded rather than carried out. Behind a
    /// `RefCell` because some arrive through `&self` methods.
    requests: RefCell<Capped<String>>,
    status_bar: Capped<(usize, String)>,
    docks: Vec<Dock>,
    toolbar_icons: Vec<i32>,
    modeless: Vec<isize>,
    clipboard: Capped<(u32, usize)>,
    /// Scintilla views made for plugins, bounded by the editor's cap.
    plugin_scintillas: usize,
    /// Registrations refused at a cap, by kind, for `Report::dropped`.
    refused: std::collections::BTreeMap<&'static str, usize>,
    removed_shortcuts: Vec<i32>,
    lang: i32,
    encoding: i32,
    format: i32,
    tabbar_hidden: bool,
    toolbar_hidden: bool,
    menu_hidden: bool,
    statusbar_hidden: bool,
    doc_switcher_shown: bool,
    line_number_mode: i32,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            requests: RefCell::new(Capped::new(MAX_REQUESTS)),
            status_bar: Capped::new(MAX_REQUESTS),
            docks: Vec::new(),
            toolbar_icons: Vec::new(),
            modeless: Vec::new(),
            clipboard: Capped::new(MAX_REQUESTS),
            plugin_scintillas: 0,
            refused: std::collections::BTreeMap::new(),
            removed_shortcuts: Vec::new(),
            lang: 0,
            encoding: UNI_COOKIE,
            format: WIN_FORMAT,
            tabbar_hidden: false,
            toolbar_hidden: false,
            menu_hidden: false,
            statusbar_hidden: false,
            doc_switcher_shown: false,
            line_number_mode: LINENUMWIDTH_DYNAMIC,
        }
    }
}

struct Dock {
    /// The plugin's window. A value only: the harness never dereferences
    /// it, and checks it with `IsWindow` before handing it back.
    h_client: isize,
    name: String,
    module: String,
    dlg_id: i32,
    shown: bool,
    /// The plugin's `tTbData`, re-read by `NPPM_DMMUPDATEDISPINFO`. The
    /// plugin promises to keep it live for as long as the registration,
    /// and the harness cannot check that promise: a plugin that
    /// registered a stack temporary makes the re-read touch freed memory.
    /// The editor keeps the same pointer and takes the same risk
    /// (DESIGN.md §6.5).
    tb_data: *const codepp_plugin_host::TbData,
}

impl Recorder {
    fn request(&self, what: String) {
        self.requests.borrow_mut().push(what);
    }

    /// Count a registration of `kind` refused at its cap.
    fn refuse(&mut self, kind: &'static str) {
        *self.refused.entry(kind).or_insert(0) += 1;
    }

    /// A plugin sent the hidden host window `WM_CLOSE`.
    pub(crate) fn request_close(&self) {
        self.request("close the host window".to_string());
    }

    /// Move what was recorded into the report.
    pub(crate) fn fill(&mut self, report: &mut Report) {
        report.requests = self.requests.get_mut().take("request", &mut report.dropped);
        report.status_bar = self.status_bar.take("status-bar", &mut report.dropped);
        report.clipboard = self.clipboard.take("clipboard", &mut report.dropped);
        report.toolbar_icons = std::mem::take(&mut self.toolbar_icons);
        report.dropped.extend(std::mem::take(&mut self.refused));
        report.docks = self
            .docks
            .iter()
            .map(|d| DockReport {
                name: d.name.clone(),
                module: d.module.clone(),
                dlg_id: d.dlg_id,
                shown: d.shown,
            })
            .collect();
    }
}

/// Whether `id` names the one buffer the harness has open.
fn is_buffer(id: isize) -> bool {
    id == BUFFER_ID
}

/// The harness as the dispatcher sees it.
pub(crate) struct Bridge<'a>(pub &'a mut Harness);

impl Bridge<'_> {
    fn main_view(&self) -> HWND {
        self.0.env.sci[0]
    }

    fn is_buffer_path(&self, path: &std::path::Path) -> bool {
        path == self.0.env.buffer_path
    }

    /// Write the main view's text to the buffer's file and mark it saved.
    fn save_buffer(&self) -> bool {
        let text = buffer_text(self.main_view());
        if std::fs::write(&self.0.env.buffer_path, text.as_bytes()).is_err() {
            return false;
        }
        super::sci_send(self.main_view(), SCI_SETSAVEPOINT, 0, 0);
        true
    }

    fn allow_window(&self, hwnd: isize, allowed: bool) {
        if let Some(allow) = &self.0.env.allow {
            allow.set(hwnd, allowed);
        }
    }

    fn dock_mut(&mut self, h_client: Hwnd) -> Option<&mut Dock> {
        self.0
            .rec
            .docks
            .iter_mut()
            .find(|d| d.h_client == h_client as isize)
    }
}

impl HostServices for Bridge<'_> {
    fn current_scintilla_hwnd(&self) -> Hwnd {
        self.main_view().0
    }

    fn scintilla_hwnd_for_view(&self, view: i32) -> Hwnd {
        match view {
            0 => self.0.env.sci[0].0,
            1 => self.0.env.sci[1].0,
            _ => core::ptr::null_mut(),
        }
    }

    fn current_buffer_id(&self) -> isize {
        BUFFER_ID
    }

    fn buffer_path(&self, id: isize) -> Option<PathBuf> {
        is_buffer(id).then(|| self.0.env.buffer_path.clone())
    }

    fn buffer_lang_type(&self, id: isize) -> i32 {
        if is_buffer(id) {
            self.0.rec.lang
        } else {
            -1
        }
    }

    fn language_name(&self, lang: i32) -> Option<&'static str> {
        LangType(lang).language_name()
    }

    fn language_desc(&self, lang: i32) -> Option<&'static str> {
        LangType(lang).language_desc()
    }

    fn current_user_lang_name(&self) -> Option<String> {
        None
    }

    fn plugins_config_dir(&self) -> PathBuf {
        self.0.env.config_dir.clone()
    }

    fn menu_handle(&self, which: i32) -> Hwnd {
        match which {
            NPPPLUGINMENU => self.0.env.plugin_menu.0,
            NPPMAINMENU => self.0.env.main_menu.0,
            _ => core::ptr::null_mut(),
        }
    }

    fn set_status_bar(&mut self, section: usize, text: String) {
        self.0.rec.status_bar.push((section, text));
    }

    fn open_file(&mut self, path: PathBuf) {
        self.0.rec.request(format!("open {}", path.display()));
    }

    fn reload_file(&mut self, path: Option<PathBuf>, with_alert: bool) {
        let target = path.unwrap_or_else(|| self.0.env.buffer_path.clone());
        self.0
            .rec
            .request(format!("reload {} alert={with_alert}", target.display()));
        if self.is_buffer_path(&target) {
            let _ = self.reload_buffer_id(BUFFER_ID, false);
        }
    }

    fn save_current_file(&mut self) -> bool {
        self.save_buffer()
    }

    fn switch_to_file(&mut self, path: PathBuf) -> bool {
        self.is_buffer_path(&path)
    }

    fn menu_command(&mut self, cmd_id: i32) {
        self.0.rec.request(format!("menu command {cmd_id}"));
        // A plugin's own command runs from the message loop, as the
        // editor posts it. A built-in command is only recorded: the
        // harness has no editor commands to run.
        if is_plugin_cmd_id(cmd_id) {
            // SAFETY: posting only enqueues.
            unsafe {
                let _ = PostMessageW(
                    Some(self.0.env.npp),
                    WM_COMMAND,
                    WPARAM(usize::try_from(cmd_id).unwrap_or(0)),
                    LPARAM(0),
                );
            }
        }
    }

    fn make_current_buffer_dirty(&mut self) {
        self.0
            .rec
            .request("make the current buffer dirty".to_string());
    }

    fn set_buffer_lang_type(&mut self, id: isize, lang: i32) -> bool {
        // The editor's rules (`shell`'s `HostBridge`): no such buffer,
        // a number in Code++'s own User Defined Language range, which no
        // plugin is given, or a negative number is refused; `L_USER`
        // keeps a buffer's UDL, so it is TRUE only for a buffer already in
        // one — never here, where the buffer is plain text.
        if !is_buffer(id) || codepp_udl::is_udl_lang_id(lang) || lang < 0 {
            return false;
        }
        if lang == L_USER {
            return self.0.rec.lang == L_USER;
        }
        self.0.rec.lang = lang;
        true
    }

    fn set_current_user_lang(&mut self, index: usize) {
        self.0.rec.request(format!("user language {index}"));
    }

    fn set_menu_item_check(&mut self, cmd_id: i32, checked: bool) {
        let flags = MF_BYCOMMAND.0
            | if checked {
                MF_CHECKED.0
            } else {
                MF_UNCHECKED.0
            };
        // SAFETY: the menu is this thread's; `CheckMenuItem` sends
        // nothing.
        unsafe {
            CheckMenuItem(
                self.0.env.main_menu,
                u32::try_from(cmd_id).unwrap_or(0),
                flags,
            );
        }
    }

    fn activate_doc(&mut self, view: i32, pos: i32) -> bool {
        view == 0 && pos == 0
    }

    fn launch_find_in_files_dialog(&mut self, directory: Option<PathBuf>, filters: Option<String>) {
        self.0.rec.request(format!(
            "find in files dir={:?} filters={filters:?}",
            directory.map(|d| d.display().to_string())
        ));
    }

    fn open_buffer_paths(&self, selector: i32) -> Vec<PathBuf> {
        if selector == codepp_plugin_host::SECOND_VIEW {
            Vec::new()
        } else {
            vec![self.0.env.buffer_path.clone()]
        }
    }

    fn current_doc_index(&self, view: i32) -> i32 {
        if view == 0 {
            0
        } else {
            -1
        }
    }

    fn buffer_encoding(&self, id: isize) -> i32 {
        if is_buffer(id) {
            self.0.rec.encoding
        } else {
            -1
        }
    }

    fn buffer_format(&self, id: isize) -> i32 {
        if is_buffer(id) {
            self.0.rec.format
        } else {
            -1
        }
    }

    fn reload_buffer_id(&mut self, id: isize, with_alert: bool) -> bool {
        if !is_buffer(id) {
            return false;
        }
        self.0
            .rec
            .request(format!("reload buffer {id} alert={with_alert}"));
        match std::fs::read(&self.0.env.buffer_path) {
            Ok(bytes) => {
                set_buffer_text(self.main_view(), &String::from_utf8_lossy(&bytes));
                true
            }
            Err(_) => false,
        }
    }

    fn set_buffer_encoding(&mut self, id: isize, unimode: i32) -> bool {
        if !is_buffer(id) {
            return false;
        }
        self.0.rec.encoding = unimode;
        true
    }

    fn set_buffer_format(&mut self, id: isize, eoltype: i32) -> bool {
        if !is_buffer(id) {
            return false;
        }
        self.0.rec.format = eoltype;
        true
    }

    fn encode_sci(&mut self, view: i32) -> i32 {
        self.0.rec.request(format!("encode view {view}"));
        self.0.rec.encoding
    }

    fn decode_sci(&mut self, view: i32) -> i32 {
        self.0.rec.request(format!("decode view {view}"));
        self.0.rec.encoding
    }

    fn is_tabbar_hidden(&self) -> bool {
        self.0.rec.tabbar_hidden
    }

    fn set_tabbar_hidden(&mut self, hidden: bool) -> bool {
        std::mem::replace(&mut self.0.rec.tabbar_hidden, hidden)
    }

    fn is_toolbar_hidden(&self) -> bool {
        self.0.rec.toolbar_hidden
    }

    fn set_toolbar_hidden(&mut self, hidden: bool) -> bool {
        std::mem::replace(&mut self.0.rec.toolbar_hidden, hidden)
    }

    fn is_menu_hidden(&self) -> bool {
        self.0.rec.menu_hidden
    }

    fn set_menu_hidden(&mut self, hidden: bool) -> bool {
        std::mem::replace(&mut self.0.rec.menu_hidden, hidden)
    }

    fn is_statusbar_hidden(&self) -> bool {
        self.0.rec.statusbar_hidden
    }

    fn set_statusbar_hidden(&mut self, hidden: bool) -> bool {
        std::mem::replace(&mut self.0.rec.statusbar_hidden, hidden)
    }

    fn save_all_files(&mut self) {
        let _ = self.save_buffer();
    }

    fn program_dir(&self) -> Option<PathBuf> {
        // Notepad++'s directory holds its `plugins` folder; so does the
        // scratch root, so a plugin that looks for its own folder
        // relative to it finds the staged copy.
        Some(self.0.env.root.clone())
    }

    fn program_path(&self) -> Option<PathBuf> {
        codepp_platform::program_path()
    }

    fn windows_version(&self) -> i32 {
        codepp_platform::windows_version_npp()
    }

    fn buffer_position(&self, id: isize) -> Option<(i32, i32)> {
        is_buffer(id).then_some((0, 0))
    }

    fn buffer_id_at(&self, view: i32, pos: i32) -> isize {
        if view == 0 && pos == 0 {
            BUFFER_ID
        } else {
            0
        }
    }

    fn save_current_as(&mut self, path: PathBuf, as_copy: bool) -> bool {
        // Outside the scratch folder; recorded, not written.
        self.0
            .rec
            .request(format!("save as {} copy={as_copy}", path.display()));
        false
    }

    fn load_session(&mut self, path: PathBuf) -> bool {
        self.0
            .rec
            .request(format!("load session {}", path.display()));
        false
    }

    fn save_current_session(&self, path: PathBuf) -> bool {
        self.0
            .rec
            .request(format!("save session {}", path.display()));
        false
    }

    fn save_session_with_files(&self, path: PathBuf, files: Vec<PathBuf>) -> bool {
        self.0.rec.request(format!(
            "save session {} with {} files",
            path.display(),
            files.len()
        ));
        false
    }

    fn read_session_file_paths(&self, path: PathBuf) -> Option<Vec<PathBuf>> {
        self.0
            .rec
            .request(format!("read session {}", path.display()));
        None
    }

    fn alloc_supported(&self) -> bool {
        true
    }

    fn allocate_cmd_id(&mut self, count: i32) -> Option<i32> {
        self.0.host.allocate_cmd_id(count)
    }

    fn allocate_marker(&mut self, count: i32) -> Option<i32> {
        self.0.host.allocate_marker(count)
    }

    fn appdata_plugins_allowed(&self) -> bool {
        true
    }

    fn current_view(&self) -> i32 {
        0
    }

    fn plugin_home_dir(&self) -> Option<PathBuf> {
        Some(self.0.env.plugins_dir.clone())
    }

    fn settings_cloud_dir(&self) -> Option<PathBuf> {
        None
    }

    fn bookmark_marker_id(&self) -> i32 {
        // Notepad++'s, and the editor's.
        24
    }

    fn editor_zoom_level(&self) -> i32 {
        i32::try_from(super::sci_send(self.main_view(), SCI_GETZOOM, 0, 0)).unwrap_or(0)
    }

    fn forward_plugin_message(
        &mut self,
        target_name: &str,
        internal_msg: i32,
        info_ptr: usize,
    ) -> isize {
        let Some(target) = self
            .0
            .host
            .iter()
            .position(|p| p.is_loaded() && p.display_label() == target_name)
            .and_then(|idx| self.0.host.message_target(idx))
        else {
            self.0
                .rec
                .request(format!("message to absent plugin {target_name:?}"));
            return 0;
        };
        // SAFETY: as in the editor's own bridge: `target` is a loaded
        // plugin's `messageProc`, run marked as that plugin, and the
        // dispatcher null-checked `info_ptr`, which the sending plugin
        // keeps live for its `SendMessage`. The bit-preserving cast is
        // the `UINT` Notepad++ passes. A message the target sends back
        // from inside finds the harness borrowed and is declined, as it
        // is in the editor.
        #[allow(clippy::cast_sign_loss)]
        unsafe {
            target.send(internal_msg as u32, info_ptr, 0)
        }
    }

    fn editor_default_fg_color(&self) -> i32 {
        i32::try_from(super::sci_send(
            self.main_view(),
            SCI_STYLEGETFORE,
            STYLE_DEFAULT,
            0,
        ))
        .unwrap_or(0)
    }

    fn editor_default_bg_color(&self) -> i32 {
        i32::try_from(super::sci_send(
            self.main_view(),
            SCI_STYLEGETBACK,
            STYLE_DEFAULT,
            0,
        ))
        .unwrap_or(0x00FF_FFFF)
    }

    fn set_smooth_font(&mut self, smooth: bool) -> bool {
        self.0.rec.request(format!("smooth font {smooth}"));
        true
    }

    fn set_editor_border_edge(&mut self, enable: bool) -> bool {
        self.0.rec.request(format!("border edge {enable}"));
        true
    }

    fn save_file(&mut self, path: PathBuf) -> bool {
        if self.is_buffer_path(&path) {
            self.save_buffer()
        } else {
            self.0.rec.request(format!("save {}", path.display()));
            false
        }
    }

    fn is_doc_switcher_shown(&self) -> bool {
        self.0.rec.doc_switcher_shown
    }

    fn set_doc_switcher_shown(&mut self, shown: bool) -> bool {
        std::mem::replace(&mut self.0.rec.doc_switcher_shown, shown)
    }

    fn doc_switcher_disable_column(&mut self, column_idx: i32, disable: bool) {
        self.0.rec.request(format!(
            "doc switcher column {column_idx} disabled={disable}"
        ));
    }

    fn line_number_width_mode(&self) -> i32 {
        self.0.rec.line_number_mode
    }

    fn set_line_number_width_mode(&mut self, mode: i32) -> bool {
        // Only the two modes Notepad++ defines; the editor refuses others.
        if !matches!(mode, LINENUMWIDTH_DYNAMIC | LINENUMWIDTH_CONSTANT) {
            return false;
        }
        self.0.rec.line_number_mode = mode;
        true
    }

    fn user_lang_count(&self) -> i32 {
        0
    }

    fn shortcut_for_cmd_id(&self, cmd_id: i32) -> Option<ShortcutKey> {
        if self.0.rec.removed_shortcuts.contains(&cmd_id) {
            return None;
        }
        self.0.host.iter().find_map(|p| {
            let index = p.func_items()?.iter().position(|f| f.cmd_id == cmd_id)?;
            p.shortcut_defaults()?.get(index).copied().flatten()
        })
    }

    fn remove_shortcut_for_cmd_id(&mut self, cmd_id: i32) -> bool {
        if self.shortcut_for_cmd_id(cmd_id).is_none() {
            return false;
        }
        self.0.rec.removed_shortcuts.push(cmd_id);
        true
    }

    fn trigger_tab_context_menu(&mut self, view: i32, tab_idx: i32) -> bool {
        self.0
            .rec
            .request(format!("tab context menu view={view} tab={tab_idx}"));
        true
    }

    fn register_modeless_dialog(&mut self, dlg: Hwnd, register: bool) -> bool {
        let dlg = dlg as isize;
        if register {
            if !self.0.rec.modeless.contains(&dlg) {
                if self.0.rec.modeless.len() >= MAX_REGISTRATIONS {
                    self.0.rec.refuse("modeless-dialog");
                    return false;
                }
                self.0.rec.modeless.push(dlg);
            }
        } else {
            self.0.rec.modeless.retain(|&d| d != dlg);
        }
        self.allow_window(dlg, register);
        true
    }

    fn create_plugin_scintilla(&mut self, parent: Hwnd) -> Hwnd {
        if self.0.rec.plugin_scintillas >= MAX_PLUGIN_SCINTILLAS {
            self.0.rec.refuse("scintilla-view");
            return core::ptr::null_mut();
        }
        let parent = HWND(parent);
        // SAFETY: `IsWindow` only reads.
        if parent.is_invalid() || !unsafe { IsWindow(Some(parent)) }.as_bool() {
            return core::ptr::null_mut();
        }
        // SAFETY: no preconditions.
        let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
            return core::ptr::null_mut();
        };
        // SAFETY: a plain child window of a live parent, on this thread.
        // Making it may send the parent `WM_PARENTNOTIFY`, i.e. run plugin
        // code while the harness is borrowed; see the module docs.
        let view = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("Scintilla"),
                windows::core::PCWSTR::null(),
                WS_CHILD | WS_CLIPCHILDREN | WS_VSCROLL | WS_HSCROLL,
                0,
                0,
                0,
                0,
                Some(parent),
                None,
                Some(instance.into()),
                None,
            )
        };
        let Ok(view) = view else {
            return core::ptr::null_mut();
        };
        // SAFETY: `view` is the Scintilla window just made.
        unsafe {
            SendMessageW(
                view,
                SCI_SETCODEPAGE,
                Some(WPARAM(SC_CP_UTF8 as usize)),
                None,
            );
        }
        self.0.rec.plugin_scintillas += 1;
        self.0
            .rec
            .request("plugin Scintilla view created".to_string());
        view.0
    }

    fn register_dock_dialog(&mut self, params: DockDialogParams) -> bool {
        let h_client = params.h_client as isize;
        // A live window, as the editor requires before it adopts one.
        // SAFETY: `IsWindow` takes any value and only reads.
        if params.h_client.is_null() || !unsafe { IsWindow(Some(HWND(params.h_client))) }.as_bool()
        {
            return false;
        }
        if self.0.rec.docks.iter().any(|d| d.h_client == h_client) {
            return false;
        }
        if self.0.rec.docks.len() >= MAX_REGISTRATIONS {
            self.0.rec.refuse("dock-panel");
            return false;
        }
        self.0.rec.docks.push(Dock {
            h_client,
            name: params.name,
            module: params.module_name,
            dlg_id: params.dlg_id,
            shown: false,
            tb_data: params.tb_data,
        });
        self.allow_window(h_client, true);
        true
    }

    fn show_dock_dialog(&mut self, h_client: Hwnd) -> bool {
        self.dock_mut(h_client).map(|d| d.shown = true).is_some()
    }

    fn hide_dock_dialog(&mut self, h_client: Hwnd) -> bool {
        self.dock_mut(h_client).map(|d| d.shown = false).is_some()
    }

    fn update_dock_disp_info(&mut self, h_client: Hwnd) -> bool {
        let Some(dock) = self.dock_mut(h_client) else {
            return false;
        };
        // SAFETY: only as safe as the plugin's promise to keep its
        // `tTbData` live for as long as the registration, which the
        // harness cannot check — see `Dock::tb_data`.
        if let Some(info) = unsafe { read_dock_disp_info(dock.tb_data) } {
            dock.name = info.name;
        }
        true
    }

    fn view_other_dock_tab(&mut self, name: &str) -> bool {
        // Exact, as the editor matches it: a wrong-case name finds nothing.
        match self.0.rec.docks.iter_mut().find(|d| d.name == name) {
            Some(dock) => {
                dock.shown = true;
                true
            }
            None => false,
        }
    }

    fn dock_hwnd_by_module(&self, module_name: &str, window_name: Option<&str>) -> Hwnd {
        self.0
            .rec
            .docks
            .iter()
            .filter(|d| answers_hwnd_lookup(&d.module, &d.name, module_name, window_name))
            // A registration outlives a window its plugin destroyed; never
            // hand back a handle Windows may since have reused.
            // SAFETY: `IsWindow` takes any value and only reads.
            .find(|d| unsafe { IsWindow(Some(HWND(d.h_client as Hwnd))) }.as_bool())
            .map_or(core::ptr::null_mut(), |d| d.h_client as Hwnd)
    }

    fn add_toolbar_icon(&mut self, cmd_id: i32, _hicon: Hwnd) -> bool {
        // A second request for a command replaces its button's image, as
        // in the editor, so it adds nothing.
        if self.0.rec.toolbar_icons.contains(&cmd_id) {
            return true;
        }
        if self.0.rec.toolbar_icons.len() >= MAX_REGISTRATIONS {
            self.0.rec.refuse("toolbar-icon");
            return false;
        }
        self.0.rec.toolbar_icons.push(cmd_id);
        true
    }

    fn is_dark_mode_enabled(&self) -> bool {
        false
    }

    fn dark_mode_colors(&self, _out: &mut NppDarkModeColors) -> bool {
        false
    }

    fn set_clipboard(&mut self, payloads: &[(u32, Vec<u8>)]) -> bool {
        // Never the real clipboard: a test run must not replace what
        // the person running it has copied.
        for (format, bytes) in payloads {
            self.0.rec.clipboard.push((*format, bytes.len()));
        }
        true
    }

    fn queue_export_save(&mut self, data: Vec<u8>, suggested_name: String, kind: u32) {
        self.0.rec.request(format!(
            "export save kind={kind} name={suggested_name:?} bytes={}",
            data.len()
        ));
    }
}
