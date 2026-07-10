//! UDL editor modal (Phase 4.6 m3).
//!
//! Modeless dialog that lets users create and edit User Defined
//! Languages, mirroring Notepad++'s UDL editor UX. Wired to the
//! Language → "User-Defined language" → "Define your language…"
//! menu item.
//!
//! # Milestone shape (m3b)
//!
//! This module lands in stages tracked in DESIGN.md §7.2 Phase
//! 4.6:
//!
//! - **m3b (this commit)** — modeless dialog opens from the menu,
//!   4-tab strip infrastructure, the first tab ("Folder & Default")
//!   is fully wired with all 7 fields (name / extensions /
//!   case-ignored / allow-fold-of-comments / fold-compact /
//!   force-pure-LC / decimal-separator), Save / Save As / Close
//!   buttons work end-to-end. Tabs 2-4 exist as placeholders so
//!   m3c-e can plug into an already-shaped shell. Save writes the
//!   UDL to `<config_dir>/userDefineLangs/` **atomically via
//!   temp-file + rename** (mirrors `shell::fif`'s pattern), warns
//!   the user if the chosen save path is outside the UDL
//!   directory, and posts
//!   [`WM_APP_UDL_REFRESH`](super::WM_APP_UDL_REFRESH) so the main
//!   window re-scans the registry and the new UDL becomes
//!   selectable from the Language menu without a restart. All
//!   nested modal pumps (`GetSaveFileNameW`, `MessageBoxW`) are
//!   guarded by the [`MODAL_PUMP_ACTIVE`] thread-local so a
//!   spurious message during the nested pump can't materialise a
//!   second `&mut UdlEditorState` overlapping the outer borrow.
//! - **m3c (this commit)** — Keywords Lists tab. Combobox selects
//!   one of the 8 keyword classes; a multi-line edit shows that
//!   class's keyword list (space-separated); a checkbox toggles
//!   the class's prefix-mode flag. Switching classes via the
//!   combobox flushes the current edit's text back into the model
//!   at the previous class index before loading the newly-
//!   selected class's saved value — protects against clobbering
//!   unsaved-in-buffer edits on a rapid Keywords 1 → Keywords 2
//!   swap.
//! - **m3d (this commit)** — Comment & Number tab. Four friendly
//!   comment fields (line marker(s), line-close style combobox,
//!   block open, block close) route through `decode_comments`
//!   / `encode_comments` — pure helpers that translate to and
//!   from the raw `NN<seq>` encoding N++'s tokeniser consumes.
//!   Seven number-config edits mirror the `numbers_*` slots
//!   verbatim.
//! - **m3e (this commit)** — Operators & Delimiters tab. Three
//!   fields: `operators1` (no-whitespace-required operators),
//!   `operators2` (whitespace-delimited operators), and
//!   `delimiters` (the 8-slot × 3-sub-part `NN<sequence>`
//!   encoding). Operators are plain space-separated tokens;
//!   delimiters are exposed as a raw multi-line edit that
//!   round-trips the encoding verbatim (including the
//!   `((EOL <chars>))` escape hatch). Building a friendly 8×3
//!   UI on top requires the udl crate to expose its tokeniser
//!   — tracked as a follow-up polish.
//! - **m3f** — Styler dialog (font / colours / nesting) launched
//!   from any tab.
//! - **m3g (deferred polish)** — live restyle on every property
//!   change (currently save-triggered rather than keystroke-
//!   triggered).
//!
//! # Layout convention
//!
//! Follows the same custom-`WS_POPUP` chrome path Find/Replace and
//! Goto use — private window class registered lazily via
//! `OnceLock`, `hbrBackground` = [`super::dialog_bg_brush`],
//! `WM_ERASEBKGND`/`WM_CTLCOLOR*` overrides to defeat the Win11
//! UxTheme override on our client area. DESIGN.md §7.4 tracks the
//! migration to the standard `#32770` dialog class as a Phase-5
//! polish item — the UDL editor migrates alongside Find/Replace
//! and Goto in that pass.

// Same rationale as the crate-root `#![allow]` block in lib.rs —
// this module is Win32 UI code that translates Rust integer
// widths into Win32 ABI shapes on every line, and the pedantic
// lint categories below flag design choices that are load-bearing
// for the dialog-plumbing pattern the whole crate uses.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    // Same "cast between raw pointers" hazard as the rest of
    // ui_win32 — Win32 hands us `*mut c_void` back where Rust
    // wants a specific-typed pointer. Individual annotations
    // would double the noise without changing semantics.
    clippy::ptr_as_ptr,
    clippy::ptr_cast_constness,
    clippy::ref_as_ptr,
    // Control-creation helpers naturally take 8-10 args (parent
    // HWND, hinst, font, id, x, y, w, text, ...). Refactoring
    // to a struct would obscure the call sites.
    clippy::too_many_arguments,
    // Same rationale as lib.rs.
    clippy::items_after_statements,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    // Win32 `w!` string literals inside doc comments produce
    // false `doc_markdown` hits on identifiers already
    // documented via the surrounding text.
    clippy::doc_markdown,
)]

use std::cell::Cell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use codepp_udl::{UdlDefinition, UdlKeywordLists, UdlSettings, UdlStyle};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    FillRect, GetStockObject, SetBkColor, DEFAULT_GUI_FONT, HDC, HFONT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, NMHDR, TCIF_TEXT, TCITEMW, TCM_ADJUSTRECT, TCM_GETCURSEL,
    TCM_INSERTITEMW, TCN_SELCHANGE, TCS_TABS, WC_COMBOBOX, WC_TABCONTROL,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetParent, GetSystemMetrics,
    GetWindowLongPtrW, IsWindow, LoadCursorW, MessageBoxW, PostMessageW, RegisterClassExW,
    SendMessageW, SetWindowLongPtrW, SetWindowPos, ShowWindow, BM_GETCHECK, BM_SETCHECK,
    BS_AUTOCHECKBOX, BS_AUTORADIOBUTTON, BS_DEFPUSHBUTTON, BS_GROUPBOX, BS_PUSHBUTTON,
    CBN_SELCHANGE, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CREATESTRUCTW,
    CS_HREDRAW, CS_VREDRAW, EN_CHANGE, ES_AUTOHSCROLL, GWLP_USERDATA, HCURSOR, HICON, HMENU,
    IDC_ARROW, IDYES, MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_YESNOCANCEL, SM_CXSCREEN,
    SM_CYSCREEN, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORBTN, WM_CTLCOLORSTATIC, WM_DESTROY, WM_ERASEBKGND,
    WM_GETTEXT, WM_GETTEXTLENGTH, WM_NCCREATE, WM_NCDESTROY, WM_NOTIFY, WM_SETTEXT, WNDCLASSEXW,
    WS_BORDER, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_CLIENTEDGE, WS_GROUP,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use crate::{apply_dialog_font, dialog_bg_brush, disable_visual_style, wide_terminated};

/// `SS_LEFT` — left-align static text. Windows-rs doesn't
/// re-export this constant (see the parallel definition around
/// lib.rs:848). Fixing the omission upstream is a separate
/// concern; the value itself is stable.
const SS_LEFT: u32 = 0x0000;

const UDL_EDITOR_CLASS: PCWSTR = w!("CodePlusPlusUdlEditorDialog");

/// Cross-thread notification the editor sends to the main window
/// after a successful save. Handled by the main window proc to
/// re-scan `Shell.udl_registry` from disk and refresh the
/// Language menu so the newly-saved UDL becomes selectable
/// without a restart.
///
/// Uses `WM_APP + 2` — `WM_APP + 1` is the cross-thread wake-up
/// message [`super::WM_APP_WAKE`]. See lib.rs for the range
/// registry.
pub(crate) const WM_APP_UDL_REFRESH: u32 = WM_APP + 2;

// --- Control ID space (dialog-local) -------------------------------

const IDC_TAB_CTRL: u16 = 100;

// Folder & Default tab
const IDC_NAME_EDIT: u16 = 200;
const IDC_EXT_EDIT: u16 = 201;
const IDC_CASE_IGNORED: u16 = 202;
const IDC_ALLOW_FOLD: u16 = 203;
const IDC_FOLD_COMPACT: u16 = 204;
const IDC_FPLC_RADIO_0: u16 = 210;
const IDC_FPLC_RADIO_1: u16 = 211;
const IDC_FPLC_RADIO_2: u16 = 212;
const IDC_DECIMAL_RADIO_0: u16 = 220;
const IDC_DECIMAL_RADIO_1: u16 = 221;
const IDC_DECIMAL_RADIO_2: u16 = 222;

// Dialog-wide buttons
const IDC_SAVE_BUTTON: u16 = 300;
const IDC_SAVE_AS_BUTTON: u16 = 301;
const IDC_CLOSE_BUTTON: u16 = 302;

// Keywords Lists tab (Phase 4.6 m3c)
const IDC_KW_CLASS_COMBO: u16 = 400;
const IDC_KW_PREFIX_CHECK: u16 = 401;
const IDC_KW_EDIT: u16 = 402;

// Comment & Number tab (Phase 4.6 m3d)
const IDC_CM_LINE_MARKER: u16 = 500;
const IDC_CM_LINE_CLOSE_COMBO: u16 = 501;
const IDC_CM_BLOCK_OPEN: u16 = 502;
const IDC_CM_BLOCK_CLOSE: u16 = 503;
const IDC_NUM_PREFIX1: u16 = 510;
const IDC_NUM_PREFIX2: u16 = 511;
const IDC_NUM_EXTRAS1: u16 = 512;
const IDC_NUM_EXTRAS2: u16 = 513;
const IDC_NUM_SUFFIX1: u16 = 514;
const IDC_NUM_SUFFIX2: u16 = 515;
const IDC_NUM_RANGE: u16 = 516;

// Operators & Delimiters tab (Phase 4.6 m3e)
const IDC_OP1_EDIT: u16 = 600;
const IDC_OP2_EDIT: u16 = 601;
const IDC_DELIMS_EDIT: u16 = 602;

thread_local! {
    /// Set true while a nested modal pump (`GetSaveFileNameW` /
    /// `MessageBoxW`) is running inside `save_action` /
    /// `confirm_discard_if_dirty`. Guards the wnd_proc handlers
    /// (`WM_COMMAND`, `WM_NOTIFY`) from re-entering the
    /// `Box<UdlEditorState>` mutable-borrow scope during the
    /// nested pump — if e.g. a spurious `TCN_SELCHANGE` arrived
    /// while `MessageBoxW` was running, the handler would
    /// otherwise materialise a second `&mut *state_ptr` overlapping
    /// the outer borrow (aliasing UB). Same discipline
    /// [`super::PLUGIN_CALL_ACTIVE`] uses on the main window for
    /// the plugin-dispatch reentrancy hazard.
    static MODAL_PUMP_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with [`MODAL_PUMP_ACTIVE`] set true. Restores the
/// previous value on drop even if `f` panics. Called around every
/// `GetSaveFileNameW` / `MessageBoxW` invocation the editor makes
/// while holding a live `&mut UdlEditorState` on the stack.
fn with_modal_pump<R>(f: impl FnOnce() -> R) -> R {
    struct Guard {
        prev: bool,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            MODAL_PUMP_ACTIVE.set(self.prev);
        }
    }
    let prev = MODAL_PUMP_ACTIVE.replace(true);
    let _guard = Guard { prev };
    f()
}

// --- Layout constants (raw pixels; see DESIGN.md §7.4 re: DPI) -----

const DIALOG_W: i32 = 560;
const DIALOG_H: i32 = 480;
const TAB_X: i32 = 8;
const TAB_Y: i32 = 8;
const TAB_H: i32 = 400;
const LABEL_H: i32 = 16;
const CTRL_H: i32 = 22;
const ROW_GAP: i32 = 8;
const GROUP_PAD: i32 = 16;
const BUTTON_W: i32 = 80;
const BUTTON_H: i32 = 26;
const BUTTON_GAP: i32 = 8;

/// How the editor was opened.
pub(crate) enum UdlEditorMode {
    /// "Define your language…" — start with a default definition,
    /// no source path. First save prompts for a filename.
    New,
    /// Editing an existing UDL loaded from the registry. The
    /// payload is boxed because `UdlDefinition` is ~kilobyte
    /// scale (24 styles + 28 keyword strings), and the `New`
    /// variant is unit-sized — without the box, every enum
    /// carries the full payload's stack footprint.
    #[allow(
        dead_code,
        reason = "m3g will wire 'Edit existing UDL' via a second menu item"
    )]
    Edit(Box<UdlEditorEditPayload>),
}

/// Payload for [`UdlEditorMode::Edit`]. Named so the pattern
/// match reads cleanly and to keep the enum variant lean.
#[allow(dead_code, reason = "m3g will use these fields")]
pub(crate) struct UdlEditorEditPayload {
    pub definition: UdlDefinition,
    pub source_path: PathBuf,
}

/// Editor state pointed to by the dialog HWND's `GWLP_USERDATA` slot.
///
/// Boxed and leaked at dialog-creation time; freed in the
/// `WM_NCDESTROY` handler after zeroing the USERDATA slot.
struct UdlEditorState {
    /// Handle to the main app window — needed so Save can
    /// `PostMessageW(WM_APP_UDL_REFRESH)` at the host.
    main_hwnd: HWND,
    /// The dialog's own HWND. Set on `WM_NCCREATE`.
    dialog: HWND,
    /// SysTabControl32 that spans the top of the dialog.
    tab_ctrl: HWND,
    /// One HWND per tab page (child of `dialog`). The active page
    /// is `SW_SHOW`-visible; others are `SW_HIDE`-hidden. Indexed
    /// 0..=3 matching tab order.
    tab_pages: [HWND; 4],
    /// Currently selected tab.
    current_tab: usize,
    /// The Folder & Default tab's control HWNDs.
    folder: FolderTabControls,
    /// The Keywords Lists tab's control HWNDs. See
    /// [`KeywordsTabControls`] for the UI shape.
    keywords: KeywordsTabControls,
    /// The Comment & Number tab's control HWNDs (Phase 4.6 m3d).
    comment_number: CommentNumberTabControls,
    /// The Operators & Delimiters tab's control HWNDs (Phase 4.6 m3e).
    operators_delimiters: OperatorsDelimitersTabControls,
    /// The in-memory UDL definition being edited. Every field
    /// change flows into here; Save serialises this and writes.
    definition: UdlDefinition,
    /// Where a plain Save writes. `None` on the "New UDL" flow
    /// until the first Save As.
    source_path: Option<PathBuf>,
    /// Set when any editable field changes since the last save.
    /// Drives the Close-time confirmation prompt.
    dirty: bool,
    /// Populated once during layout to gate `WM_COMMAND`
    /// handling — control creation itself fires `EN_CHANGE`
    /// notifications for the initial edit-box text set by
    /// `WM_SETTEXT`, and processing those before layout finishes
    /// would fire the "dirty" bit on the newly-loaded UDL.
    controls_ready: bool,
}

struct FolderTabControls {
    name_edit: HWND,
    ext_edit: HWND,
    case_ignored: HWND,
    allow_fold: HWND,
    fold_compact: HWND,
    fplc_radios: [HWND; 3],
    decimal_radios: [HWND; 3],
    save_btn: HWND,
    save_as_btn: HWND,
    close_btn: HWND,
}

/// Controls for the Comment & Number tab (Phase 4.6 m3d).
///
/// The comment section presents four *friendly* fields (line
/// markers, line-close style combobox, block open, block close)
/// rather than the raw `NN<seq>` encoding — see
/// [`decode_comments`] / [`encode_comments`] for the pure
/// (String ↔ DecodedComments) helpers that the wnd_proc arms
/// route through.
///
/// The number section is 7 plain edit boxes, each mirroring one
/// slot of [`codepp_udl::UdlKeywordLists`]. Numbers use no
/// encoding — the raw string content is what N++ writes and what
/// the tokeniser consumes.
struct CommentNumberTabControls {
    line_marker: HWND,
    line_close_combo: HWND,
    block_open: HWND,
    block_close: HWND,
    num_prefix1: HWND,
    num_prefix2: HWND,
    num_extras1: HWND,
    num_extras2: HWND,
    num_suffix1: HWND,
    num_suffix2: HWND,
    num_range: HWND,
}

/// Controls for the Operators & Delimiters tab (Phase 4.6 m3e).
///
/// # Design note
///
/// This tab exposes the raw N++-encoded strings directly rather
/// than parsing them into structured fields, which is a
/// deliberate trade-off:
///
/// - **Operators1** and **Operators2** are plain space-separated
///   token lists — no encoding — so a single edit box holds them
///   verbatim.
/// - **Delimiters** use the compact `NN<sequence>` encoding with
///   8 slots × 3 sub-parts (open / escape / close, indices 00-23)
///   and support the special `((EOL <chars>))` escape hatch that
///   embeds a space inside a token (see
///   [`codepp_udl::rules`]'s `tokenise_udl_encoding`). Building a
///   friendly 8×3 UI on top of that encoding requires a
///   crate-private tokeniser we don't yet expose, and getting the
///   `((EOL <chars>))` round-trip byte-clean is fiddly. Exposing
///   the raw encoding in a multi-line edit ships correctly and
///   gives the user full expressive power without risking a
///   round-trip that silently loses information for real-world
///   fixtures (Markdown / Bash / SQL all use `((EOL <chars>))`).
///
/// A friendly-UI m3e polish pass is a natural follow-up once the
/// tokeniser exposes its parser to callers.
#[allow(
    clippy::struct_field_names,
    reason = "the `_edit` suffix communicates HWND-of-edit-box; \
              stripping it leaves ambiguous names like `op1` / `delims`"
)]
struct OperatorsDelimitersTabControls {
    op1_edit: HWND,
    op2_edit: HWND,
    delims_edit: HWND,
}

/// Controls for the Keywords Lists tab (Phase 4.6 m3c).
///
/// Layout matches Notepad++'s UDL editor: a combobox picks one of
/// the 8 keyword classes; the multi-line edit box below shows the
/// selected class's keyword list; a checkbox next to the combobox
/// toggles prefix mode for the selected class. Switching classes
/// via the combobox flushes the edit's current text back into the
/// model at the previous class index, then re-populates the edit
/// from the model at the newly-selected index.
struct KeywordsTabControls {
    /// Drop-down list ("Keywords 1" .. "Keywords 8"). Only the
    /// selection index matters; labels are cosmetic.
    class_combo: HWND,
    /// "Prefix mode (this class)" checkbox — mirrors
    /// [`UdlDefinition::prefix`]`[current_class]`.
    prefix_check: HWND,
    /// Multi-line edit box holding the selected class's keyword
    /// list (space-separated tokens). Committed back to the
    /// model on `EN_CHANGE` at the current-class index; refreshed
    /// on combobox `CBN_SELCHANGE`.
    edit: HWND,
    /// Which of the 8 keyword classes (0-indexed) the edit box
    /// is currently displaying. On combobox selection change,
    /// the edit's text is written to `keyword_lists.keywords[
    /// current_class]` before this index is updated to the new
    /// selection.
    current_class: usize,
}

/// Register the UDL editor's private window class. Called from
/// [`show_udl_editor`]; the `OnceLock` ensures we register only
/// once even across repeated open/close cycles.
fn register_class(hinst: HINSTANCE) {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| unsafe {
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or(HCURSOR::default());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(udl_editor_wnd_proc),
            hInstance: hinst,
            hCursor: cursor,
            hbrBackground: dialog_bg_brush(),
            hIcon: HICON::default(),
            lpszClassName: UDL_EDITOR_CLASS,
            ..Default::default()
        };
        let atom = RegisterClassExW(&raw const wc);
        if atom == 0 {
            tracing::error!("failed to register UDL editor window class");
        }
    });
}

/// Open the UDL editor dialog.
///
/// If `existing` is `Some(hwnd)` and the HWND is still live, the
/// editor is brought to front rather than recreated — matches the
/// Find/Replace pattern (see lib.rs:21002).
///
/// Returns the dialog's HWND on success. The caller stashes this
/// on `WindowState.udl_editor_dlg` so the main pump can route
/// `IsDialogMessageW` to it and so a second click on "Define your
/// language…" reuses the same dialog.
pub(crate) fn show_udl_editor(
    main_hwnd: HWND,
    existing: Option<HWND>,
    mode: UdlEditorMode,
) -> Option<HWND> {
    // Reuse fast path: if the dialog is already open, foreground
    // it rather than opening a second copy.
    if let Some(hwnd) = existing {
        if unsafe { IsWindow(Some(hwnd)).as_bool() } {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
            }
            return Some(hwnd);
        }
    }

    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }.ok()?.into();
    register_class(hinst);

    // Prepare the initial state before creating the window so the
    // `WM_NCCREATE` handler can stash it via `lpCreateParams`.
    let (definition, source_path) = match mode {
        UdlEditorMode::New => (default_new_udl(), None),
        UdlEditorMode::Edit(payload) => {
            let UdlEditorEditPayload {
                definition,
                source_path,
            } = *payload;
            (definition, Some(source_path))
        }
    };
    let boxed = Box::new(UdlEditorState {
        main_hwnd,
        dialog: HWND::default(),
        tab_ctrl: HWND::default(),
        tab_pages: [HWND::default(); 4],
        current_tab: 0,
        folder: FolderTabControls {
            name_edit: HWND::default(),
            ext_edit: HWND::default(),
            case_ignored: HWND::default(),
            allow_fold: HWND::default(),
            fold_compact: HWND::default(),
            fplc_radios: [HWND::default(); 3],
            decimal_radios: [HWND::default(); 3],
            save_btn: HWND::default(),
            save_as_btn: HWND::default(),
            close_btn: HWND::default(),
        },
        keywords: KeywordsTabControls {
            class_combo: HWND::default(),
            prefix_check: HWND::default(),
            edit: HWND::default(),
            current_class: 0,
        },
        comment_number: CommentNumberTabControls {
            line_marker: HWND::default(),
            line_close_combo: HWND::default(),
            block_open: HWND::default(),
            block_close: HWND::default(),
            num_prefix1: HWND::default(),
            num_prefix2: HWND::default(),
            num_extras1: HWND::default(),
            num_extras2: HWND::default(),
            num_suffix1: HWND::default(),
            num_suffix2: HWND::default(),
            num_range: HWND::default(),
        },
        operators_delimiters: OperatorsDelimitersTabControls {
            op1_edit: HWND::default(),
            op2_edit: HWND::default(),
            delims_edit: HWND::default(),
        },
        definition,
        source_path,
        dirty: false,
        controls_ready: false,
    });
    let state_ptr = Box::into_raw(boxed);

    let title = wide_terminated("User Defined Language");
    let (x, y) = center_on_screen(DIALOG_W, DIALOG_H);
    let dlg = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            UDL_EDITOR_CLASS,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            x,
            y,
            DIALOG_W,
            DIALOG_H,
            Some(main_hwnd),
            None,
            Some(hinst),
            Some(state_ptr as *mut c_void),
        )
    };
    let Ok(dlg) = dlg else {
        // Creation failed. We intentionally do NOT reclaim
        // `state_ptr` here — the pointer was handed to Windows as
        // `lpCreateParams`, and `WM_NCCREATE` may have already
        // stashed it into `GWLP_USERDATA`. On a partial-init
        // failure where `WM_CREATE` returns `-1` (or Windows
        // aborts creation mid-way after `WM_NCCREATE` succeeds —
        // documented on MSDN, but rare in practice), Windows
        // synthesises a `WM_NCDESTROY` before `CreateWindowExW`
        // returns, and our `WM_NCDESTROY` arm already reclaims
        // the box. A second `Box::from_raw` here would double-
        // free — a heap-corruption primitive we can't accept.
        //
        // The trade-off is a one-`UdlEditorState`-Box leak on
        // the exceedingly rare creation-failure path. This
        // matches the convention every other sibling dialog in
        // ui_win32 uses (find_replace, goto, about, style_config,
        // color_picker) — see the m3b security-audit finding
        // referenced in DESIGN.md §7.4.
        tracing::error!("CreateWindowExW failed for UDL editor");
        return None;
    };

    unsafe {
        let _ = ShowWindow(dlg, SW_SHOW);
    }
    Some(dlg)
}

/// Compute the top-left corner for a dialog of `(w, h)` centered
/// on the primary monitor. Matches the sizing convention every
/// other dialog in ui_win32 uses (see e.g. `show_goto_dialog`).
fn center_on_screen(w: i32, h: i32) -> (i32, i32) {
    unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let x = (sw - w).max(0) / 2;
        let y = (sh - h).max(0) / 2;
        (x, y)
    }
}

/// The default in-memory UDL for the "New UDL" flow. Matches N++
/// defaults: unnamed, no extensions, all-off settings, empty
/// keyword lists, one style entry per name (`DEFAULT` etc.) with
/// N++-conventional light-mode colours.
pub(crate) fn default_new_udl() -> UdlDefinition {
    UdlDefinition {
        name: "new user defined language".to_owned(),
        extensions: Vec::new(),
        udl_version: "2.1".to_owned(),
        dark_mode_theme: false,
        settings: UdlSettings {
            case_ignored: false,
            allow_fold_of_comments: false,
            fold_compact: false,
            force_pure_lc: 0,
            decimal_separator: 0,
        },
        prefix: [false; 8],
        keyword_lists: UdlKeywordLists::default(),
        styles: default_style_slots(),
        source_path: None,
        preamble: None,
    }
}

/// The 24 named style slots every N++ UDL carries, initialised to
/// black-on-white with no font-style bits. The m3f Styler dialog
/// lets users customise each; until then a New UDL just has these
/// safe defaults so the write-and-reload path produces a well-
/// formed file.
fn default_style_slots() -> Vec<UdlStyle> {
    const SLOT_NAMES: &[&str] = &[
        "DEFAULT",
        "COMMENTS",
        "LINE COMMENTS",
        "NUMBERS",
        "KEYWORDS1",
        "KEYWORDS2",
        "KEYWORDS3",
        "KEYWORDS4",
        "KEYWORDS5",
        "KEYWORDS6",
        "KEYWORDS7",
        "KEYWORDS8",
        "OPERATORS",
        "FOLDER IN CODE1",
        "FOLDER IN CODE2",
        "FOLDER IN COMMENT",
        "DELIMITERS1",
        "DELIMITERS2",
        "DELIMITERS3",
        "DELIMITERS4",
        "DELIMITERS5",
        "DELIMITERS6",
        "DELIMITERS7",
        "DELIMITERS8",
    ];
    SLOT_NAMES
        .iter()
        .map(|name| UdlStyle {
            name: (*name).to_owned(),
            fg_color: 0x0000_0000,
            bg_color: 0x00FF_FFFF,
            font_name: String::new(),
            font_style: 0,
            nesting: 0,
        })
        .collect()
}

// -------------------------------------------------------------
// Window proc
// -------------------------------------------------------------

/// Subclass ID for the tab-page forwarder. Any unique-per-HWND
/// integer value works; using a distinct constant so a future
/// second subclass on the same HWND doesn't collide.
const TAB_PAGE_SUBCLASS_ID: usize = 0x7564_6C31; // "udl1" — mnemonic

/// `SUBCLASSPROC` installed on each tab-page STATIC window.
///
/// Forwards `WM_COMMAND` and `WM_NOTIFY` up to the tab page's
/// parent (the editor dialog). Everything else falls through to
/// `DefSubclassProc`, which chains to the original STATIC WndProc
/// so paint / theming / accessibility keep working.
///
/// # Why this exists
///
/// Win32 delivers control notifications to the DIRECT parent —
/// which for our tab-page-child controls is the tab-page STATIC
/// window, not the dialog. STATIC's own WndProc drops `WM_COMMAND`
/// silently; without this forwarder, every keystroke on the Name
/// edit, every combobox selection, every checkbox click on
/// Folder & Default / Keywords Lists would never reach
/// `udl_editor_wnd_proc`'s `WM_COMMAND` arm — the field controls
/// would be dead-on-arrival.
extern "system" fn tab_page_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id_subclass: usize,
    _ref_data: usize,
) -> LRESULT {
    // Same panic-catch discipline as `udl_editor_wnd_proc` —
    // unwinding across an `extern "system"` frame is UB.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        match msg {
            WM_COMMAND | WM_NOTIFY => {
                if let Ok(parent) = GetParent(hwnd) {
                    return SendMessageW(parent, msg, Some(wparam), Some(lparam));
                }
                LRESULT(0)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }));
    if let Ok(lr) = result {
        lr
    } else {
        tracing::error!("panic caught in tab_page_subclass_proc");
        LRESULT(0)
    }
}

extern "system" fn udl_editor_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        match msg {
            WM_NCCREATE => {
                let cs = lparam.0 as *const CREATESTRUCTW;
                if !cs.is_null() {
                    let state_ptr = (*cs).lpCreateParams as isize;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CREATE => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut UdlEditorState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    state.dialog = hwnd;
                    build_controls(state);
                    populate_folder_tab(state);
                    populate_keywords_tab(state);
                    populate_comment_number_tab(state);
                    populate_operators_delimiters_tab(state);
                    state.controls_ready = true;
                }
                LRESULT(0)
            }
            WM_ERASEBKGND => {
                // Paint the client area ourselves so Win11
                // UxTheme doesn't override to the system dialog
                // colour. Matches the pattern in
                // `find_replace_wnd_proc` (lib.rs:19850).
                let hdc = HDC(wparam.0 as *mut c_void);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &raw mut rc);
                let _ = FillRect(hdc, &raw const rc, dialog_bg_brush());
                LRESULT(1)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let hdc = HDC(wparam.0 as *mut c_void);
                let _ = SetBkColor(hdc, COLORREF(DIALOG_BG_LOCAL));
                LRESULT(dialog_bg_brush().0 as isize)
            }
            WM_NOTIFY => {
                // Re-entrancy guard: if a nested modal pump is
                // running (Save-As / dirty-prompt), bail so we
                // don't materialise a second `&mut *state_ptr`
                // overlapping the outer borrow.
                if MODAL_PUMP_ACTIVE.get() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let nmhdr = lparam.0 as *const NMHDR;
                if !nmhdr.is_null() {
                    let id = (*nmhdr).idFrom as u16;
                    let code = (*nmhdr).code;
                    if id == IDC_TAB_CTRL && code == TCN_SELCHANGE {
                        let state_ptr =
                            GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut UdlEditorState;
                        if !state_ptr.is_null() {
                            let state = &mut *state_ptr;
                            let sel =
                                SendMessageW(state.tab_ctrl, TCM_GETCURSEL, None, None).0 as isize;
                            if sel >= 0 && (sel as usize) < state.tab_pages.len() {
                                switch_tab(state, sel as usize);
                            }
                        }
                    }
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                // Same guard rationale as `WM_NOTIFY`.
                if MODAL_PUMP_ACTIVE.get() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut UdlEditorState;
                if state_ptr.is_null() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let state = &mut *state_ptr;
                if !state.controls_ready {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                handle_command(state, wparam, lparam);
                LRESULT(0)
            }
            WM_CLOSE => {
                if MODAL_PUMP_ACTIVE.get() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut UdlEditorState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if !confirm_discard_if_dirty(state) {
                        return LRESULT(0);
                    }
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                // Notify the main window so it can clear its
                // `WindowState.udl_editor_dlg` slot.
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut UdlEditorState;
                if !state_ptr.is_null() {
                    let state = &*state_ptr;
                    let _ = PostMessageW(
                        Some(state.main_hwnd),
                        WM_APP_UDL_CLOSED,
                        WPARAM(0),
                        LPARAM(hwnd.0 as isize),
                    );
                }
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let state_ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut UdlEditorState;
                if !state_ptr.is_null() {
                    drop(Box::from_raw(state_ptr));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }));
    if let Ok(lr) = result {
        lr
    } else {
        // Panic across FFI is UB. Log, absorb, return zero so
        // Windows continues processing.
        tracing::error!("panic caught in udl_editor_wnd_proc");
        LRESULT(0)
    }
}

/// Local copy of the DIALOG_BG constant. Used by
/// `WM_CTLCOLORSTATIC`/`WM_CTLCOLORBTN` because the
/// `SetBkColor` expects a `COLORREF` matching what the brush
/// paints so text backgrounds don't show a 1-pixel colour
/// mismatch.
const DIALOG_BG_LOCAL: u32 = crate::DIALOG_BG;

/// Sent by [`udl_editor_wnd_proc`]'s `WM_DESTROY` handler to the
/// main window on dialog close, so the main window can clear the
/// `udl_editor_dlg: Option<HWND>` slot it stashed at
/// `show_udl_editor` time. Otherwise a stale HWND would sit there
/// until the app quits, and the reuse fast path would try to
/// re-activate a destroyed window.
pub(crate) const WM_APP_UDL_CLOSED: u32 = WM_APP + 3;

// -------------------------------------------------------------
// Control creation + layout
// -------------------------------------------------------------

fn build_controls(state: &mut UdlEditorState) {
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .ok()
        .map_or(HINSTANCE::default(), HINSTANCE::from);
    let font: HFONT = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };

    // Tab strip.
    state.tab_ctrl = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WC_TABCONTROL,
            None,
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN | WINDOW_STYLE(TCS_TABS),
            TAB_X,
            TAB_Y,
            DIALOG_W - 2 * TAB_X - 16,
            TAB_H,
            Some(state.dialog),
            Some(HMENU(IDC_TAB_CTRL as usize as *mut c_void)),
            Some(hinst),
            None,
        )
    }
    .unwrap_or(HWND::default());
    unsafe {
        apply_dialog_font(state.tab_ctrl, font);
    }

    // Insert 4 tabs. Tabs 2-4 are placeholder shells for m3c-e.
    let tab_labels = [
        "Folder && Default",
        "Keywords Lists",
        "Comment && Number",
        "Operators && Delimiters",
    ];
    for (i, label) in tab_labels.iter().enumerate() {
        let wide = wide_terminated(label);
        let item = TCITEMW {
            mask: TCIF_TEXT,
            pszText: PWSTR(wide.as_ptr() as *mut u16),
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                state.tab_ctrl,
                TCM_INSERTITEMW,
                Some(WPARAM(i)),
                Some(LPARAM(&raw const item as isize)),
            );
        }
    }

    // Compute the tab-content rectangle by asking the tab
    // control to adjust our rect. Same call TCM_ADJUSTRECT
    // documents.
    let mut content_rc = RECT::default();
    let _ = unsafe { GetClientRect(state.tab_ctrl, &raw mut content_rc) };
    unsafe {
        SendMessageW(
            state.tab_ctrl,
            TCM_ADJUSTRECT,
            Some(WPARAM(0)),
            Some(LPARAM(&raw mut content_rc as isize)),
        );
    }
    // The rect is in tab-control coords; translate to dialog
    // coords by adding tab's (TAB_X, TAB_Y) origin.
    content_rc.left += TAB_X;
    content_rc.right += TAB_X;
    content_rc.top += TAB_Y;
    content_rc.bottom += TAB_Y;

    // Create the 4 tab-page containers (each a plain STATIC
    // window sized to `content_rc`). Only page 0 starts visible;
    // switching tabs flips visibility.
    //
    // Immediately after creation we subclass each page's WndProc
    // to `tab_page_wnd_proc` — this forwards `WM_COMMAND` and
    // `WM_NOTIFY` up to the dialog. Without the forwarder, Win32
    // sends control notifications to the direct parent, which is
    // the tab page (STATIC-class), whose `DefWindowProc` drops
    // `WM_COMMAND` on the floor. The dialog's WndProc never sees
    // the notification and the field controls are dead-on-arrival
    // — every keystroke on the Name edit, every combobox
    // selection, silently vanishes. The forwarder fixes that by
    // routing the two notification-carrying messages to the
    // dialog, which is where our handlers live.
    for i in 0..4 {
        let page = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                None,
                WS_CHILD
                    | WS_CLIPCHILDREN
                    | WS_CLIPSIBLINGS
                    | if i == 0 { WS_VISIBLE } else { WINDOW_STYLE(0) }
                    | WINDOW_STYLE(SS_LEFT),
                content_rc.left,
                content_rc.top,
                content_rc.right - content_rc.left,
                content_rc.bottom - content_rc.top,
                Some(state.dialog),
                None,
                Some(hinst),
                None,
            )
        }
        .unwrap_or(HWND::default());
        state.tab_pages[i] = page;
        // Install the WM_COMMAND / WM_NOTIFY forwarder subclass.
        // `SetWindowSubclass` returns BOOL — failure is
        // extremely rare (only on out-of-comctl32-resources) but
        // if it does fail this tab's field controls become
        // silently inert (WM_COMMAND won't reach the dialog).
        // Log it so a user hitting the rare failure sees a
        // diagnostic instead of dead keystrokes.
        let ok = unsafe {
            SetWindowSubclass(page, Some(tab_page_subclass_proc), TAB_PAGE_SUBCLASS_ID, 0)
        };
        if !ok.as_bool() {
            tracing::warn!(
                tab_index = i,
                "SetWindowSubclass failed for UDL editor tab page; \
                 field controls on this tab will be inert"
            );
        }
    }

    // All 4 tabs now have real controls — no placeholder pass
    // required after m3e.

    // Build Folder & Default tab controls (page 0).
    build_folder_tab(state, hinst, font);
    // Build Keywords Lists tab controls (page 1) — Phase 4.6 m3c.
    build_keywords_tab(state, hinst, font);
    // Build Comment & Number tab controls (page 2) — Phase 4.6 m3d.
    build_comment_number_tab(state, hinst, font);
    // Build Operators & Delimiters tab controls (page 3) — Phase 4.6 m3e.
    build_operators_delimiters_tab(state, hinst, font);
    // Dialog-wide bottom-row buttons (Save / Save As / Close).
    build_bottom_buttons(state, hinst, font);
}

fn build_folder_tab(state: &mut UdlEditorState, hinst: HINSTANCE, font: HFONT) {
    let parent = state.tab_pages[0];

    let mut y = 16;
    let content_w = 380;
    let label_x = 16;
    let field_x = 16;
    let field_w = content_w - 32;

    // Name label + edit
    let _ = static_label(parent, hinst, font, label_x, y, field_w, "Name:");
    y += LABEL_H + 2;
    state.folder.name_edit = edit_box(parent, hinst, font, IDC_NAME_EDIT, field_x, y, field_w);
    y += CTRL_H + ROW_GAP;

    // Extensions label + edit
    let _ = static_label(
        parent,
        hinst,
        font,
        label_x,
        y,
        field_w,
        "Ext. (separate by space):",
    );
    y += LABEL_H + 2;
    state.folder.ext_edit = edit_box(parent, hinst, font, IDC_EXT_EDIT, field_x, y, field_w);
    y += CTRL_H + ROW_GAP + 4;

    // Case-ignored checkbox
    state.folder.case_ignored = check_box(
        parent,
        hinst,
        font,
        IDC_CASE_IGNORED,
        label_x,
        y,
        field_w,
        "Ignore case",
    );
    y += CTRL_H + ROW_GAP;

    // Allow fold of comments
    state.folder.allow_fold = check_box(
        parent,
        hinst,
        font,
        IDC_ALLOW_FOLD,
        label_x,
        y,
        field_w,
        "Allow folding of comments",
    );
    y += CTRL_H + ROW_GAP;

    // Fold compact
    state.folder.fold_compact = check_box(
        parent,
        hinst,
        font,
        IDC_FOLD_COMPACT,
        label_x,
        y,
        field_w,
        "Fold compact (fold empty lines with the enclosing block)",
    );
    y += CTRL_H + ROW_GAP + 8;

    // "Force pure line comment" radio group
    let _ = group_box(
        parent,
        hinst,
        font,
        label_x,
        y,
        field_w,
        3 * CTRL_H + 20,
        "Line-comment position:",
    );
    let g_y = y + 20;
    let fplc_labels = [
        "Allow anywhere on the line",
        "Force at start of line",
        "Allow preceding whitespace only",
    ];
    let fplc_ids = [IDC_FPLC_RADIO_0, IDC_FPLC_RADIO_1, IDC_FPLC_RADIO_2];
    for i in 0..3 {
        state.folder.fplc_radios[i] = radio_button(
            parent,
            hinst,
            font,
            fplc_ids[i],
            label_x + GROUP_PAD,
            g_y + i as i32 * CTRL_H,
            field_w - GROUP_PAD * 2,
            fplc_labels[i],
            i == 0,
        );
    }
    y += 3 * CTRL_H + 30;

    // Decimal separator radio group
    let _ = group_box(
        parent,
        hinst,
        font,
        label_x,
        y,
        field_w,
        3 * CTRL_H + 20,
        "Decimal separator:",
    );
    let g_y = y + 20;
    let dec_labels = ["Dot only", "Comma only", "Both accepted"];
    let dec_ids = [
        IDC_DECIMAL_RADIO_0,
        IDC_DECIMAL_RADIO_1,
        IDC_DECIMAL_RADIO_2,
    ];
    for i in 0..3 {
        state.folder.decimal_radios[i] = radio_button(
            parent,
            hinst,
            font,
            dec_ids[i],
            label_x + GROUP_PAD,
            g_y + i as i32 * CTRL_H,
            field_w - GROUP_PAD * 2,
            dec_labels[i],
            i == 0,
        );
    }
}

fn build_keywords_tab(state: &mut UdlEditorState, hinst: HINSTANCE, font: HFONT) {
    let parent = state.tab_pages[1];

    // Row 1: class selector + prefix checkbox.
    let _ = static_label(parent, hinst, font, 16, 20, 100, "Keyword class:");
    state.keywords.class_combo = combo_box(
        parent,
        hinst,
        font,
        IDC_KW_CLASS_COMBO,
        120,
        18,
        180,
        // Dropdown-list height needs the closed size PLUS the
        // extended drop area — 200px is enough to show all 8
        // items without scrolling.
        200,
    );
    // Populate the combobox with the 8 class labels. We NEVER
    // want to fire a `CBN_SELCHANGE` for these programmatic
    // inserts, so use the modal-pump guard to short-circuit any
    // wnd_proc re-entry the edit control might trigger.
    with_modal_pump(|| {
        for i in 1..=8_u32 {
            let label = format!("Keywords {i}");
            let wide = wide_terminated(&label);
            unsafe {
                SendMessageW(
                    state.keywords.class_combo,
                    CB_ADDSTRING,
                    None,
                    Some(LPARAM(wide.as_ptr() as isize)),
                );
            }
        }
    });

    state.keywords.prefix_check = check_box(
        parent,
        hinst,
        font,
        IDC_KW_PREFIX_CHECK,
        320,
        20,
        200,
        "Prefix mode (this class)",
    );

    // Row 2: label above the multi-line edit.
    let _ = static_label(
        parent,
        hinst,
        font,
        16,
        52,
        400,
        "Keywords (space-separated):",
    );

    // Row 3: multi-line edit filling the rest of the tab page.
    state.keywords.edit = multi_line_edit(parent, hinst, font, IDC_KW_EDIT, 16, 72, 500, 280);
}

fn build_comment_number_tab(state: &mut UdlEditorState, hinst: HINSTANCE, font: HFONT) {
    let parent = state.tab_pages[2];
    let mut y = 16;

    // --- Comment section ---
    let _ = static_label(
        parent,
        hinst,
        font,
        16,
        y,
        400,
        "Line-comment marker(s) (space-separated):",
    );
    y += LABEL_H + 2;
    state.comment_number.line_marker =
        edit_box(parent, hinst, font, IDC_CM_LINE_MARKER, 16, y, 500);
    y += CTRL_H + ROW_GAP;

    let _ = static_label(parent, hinst, font, 16, y, 200, "Line-comment closes at:");
    state.comment_number.line_close_combo = combo_box(
        parent,
        hinst,
        font,
        IDC_CM_LINE_CLOSE_COMBO,
        220,
        y - 2,
        180,
        160,
    );
    // Populate the 4 line-close options in the same order as
    // `LineCloseStyle` variants (None, Eol, Eof, Both) — the
    // `IDC_CM_LINE_CLOSE_COMBO` arm assumes the sel index matches
    // the discriminant order.
    with_modal_pump(|| {
        for label in [
            "(no close marker)",
            "End of line",
            "End of file",
            "Both EOL and EOF",
        ] {
            let wide = wide_terminated(label);
            unsafe {
                SendMessageW(
                    state.comment_number.line_close_combo,
                    CB_ADDSTRING,
                    None,
                    Some(LPARAM(wide.as_ptr() as isize)),
                );
            }
        }
    });
    y += CTRL_H + ROW_GAP;

    let _ = static_label(parent, hinst, font, 16, y, 200, "Block-comment open:");
    state.comment_number.block_open =
        edit_box(parent, hinst, font, IDC_CM_BLOCK_OPEN, 220, y - 2, 180);
    y += CTRL_H + ROW_GAP;

    let _ = static_label(parent, hinst, font, 16, y, 200, "Block-comment close:");
    state.comment_number.block_close =
        edit_box(parent, hinst, font, IDC_CM_BLOCK_CLOSE, 220, y - 2, 180);
    y += CTRL_H + ROW_GAP + 12;

    // --- Number section ---
    let _ = static_label(parent, hinst, font, 16, y, 200, "Number prefix set 1:");
    state.comment_number.num_prefix1 =
        edit_box(parent, hinst, font, IDC_NUM_PREFIX1, 220, y - 2, 180);
    y += CTRL_H + ROW_GAP;

    let _ = static_label(parent, hinst, font, 16, y, 200, "Number prefix set 2:");
    state.comment_number.num_prefix2 =
        edit_box(parent, hinst, font, IDC_NUM_PREFIX2, 220, y - 2, 180);
    y += CTRL_H + ROW_GAP;

    let _ = static_label(parent, hinst, font, 16, y, 200, "Number extras set 1:");
    state.comment_number.num_extras1 =
        edit_box(parent, hinst, font, IDC_NUM_EXTRAS1, 220, y - 2, 180);
    y += CTRL_H + ROW_GAP;

    let _ = static_label(parent, hinst, font, 16, y, 200, "Number extras set 2:");
    state.comment_number.num_extras2 =
        edit_box(parent, hinst, font, IDC_NUM_EXTRAS2, 220, y - 2, 180);
    y += CTRL_H + ROW_GAP;

    let _ = static_label(parent, hinst, font, 16, y, 200, "Number suffix set 1:");
    state.comment_number.num_suffix1 =
        edit_box(parent, hinst, font, IDC_NUM_SUFFIX1, 220, y - 2, 180);
    y += CTRL_H + ROW_GAP;

    let _ = static_label(parent, hinst, font, 16, y, 200, "Number suffix set 2:");
    state.comment_number.num_suffix2 =
        edit_box(parent, hinst, font, IDC_NUM_SUFFIX2, 220, y - 2, 180);
    y += CTRL_H + ROW_GAP;

    let _ = static_label(parent, hinst, font, 16, y, 200, "Number range operator:");
    state.comment_number.num_range = edit_box(parent, hinst, font, IDC_NUM_RANGE, 220, y - 2, 180);
}

fn build_operators_delimiters_tab(state: &mut UdlEditorState, hinst: HINSTANCE, font: HFONT) {
    let parent = state.tab_pages[3];
    let mut y = 16;
    let field_w = 500;

    // Operators 1 (no whitespace required)
    let _ = static_label(
        parent,
        hinst,
        font,
        16,
        y,
        field_w,
        "Operators 1 — no whitespace required (space-separated):",
    );
    y += LABEL_H + 2;
    state.operators_delimiters.op1_edit =
        edit_box(parent, hinst, font, IDC_OP1_EDIT, 16, y, field_w);
    y += CTRL_H + ROW_GAP;

    // Operators 2 (whitespace-required)
    let _ = static_label(
        parent,
        hinst,
        font,
        16,
        y,
        field_w,
        "Operators 2 — whitespace-delimited (space-separated):",
    );
    y += LABEL_H + 2;
    state.operators_delimiters.op2_edit =
        edit_box(parent, hinst, font, IDC_OP2_EDIT, 16, y, field_w);
    y += CTRL_H + ROW_GAP + 8;

    // Delimiters (raw N++ encoding)
    let _ = static_label(
        parent,
        hinst,
        font,
        16,
        y,
        field_w,
        "Delimiters (raw N++ encoding — space-separated NN<sequence> tokens):",
    );
    y += LABEL_H + 2;
    // Delimiter edit height: 210px. Trimmed from an initial 220 so
    // the trailing hint label below fits inside the tab content
    // rect — the tab-strip row plus TCM_ADJUSTRECT insets consume
    // enough of the 400px TAB_H that 220 + hint could truncate
    // the last few pixels of the hint on default DPI.
    state.operators_delimiters.delims_edit =
        multi_line_edit(parent, hinst, font, IDC_DELIMS_EDIT, 16, y, field_w, 210);
    y += 210 + 4;

    // Help hint below the delimiters edit.
    let _ = static_label(
        parent,
        hinst,
        font,
        16,
        y,
        field_w,
        "Format: 8 slots × (open / escape / close) indexed 00-23. E.g. 00\" 01\\ 02\" for double-quoted strings.",
    );
}

fn build_bottom_buttons(state: &mut UdlEditorState, hinst: HINSTANCE, font: HFONT) {
    let y = DIALOG_H - BUTTON_H - 16 - 26; // 26 = title-bar overhead
    let total_w = 3 * BUTTON_W + 2 * BUTTON_GAP;
    let mut x = (DIALOG_W - total_w) / 2;
    state.folder.save_btn = push_button(
        state.dialog,
        hinst,
        font,
        IDC_SAVE_BUTTON,
        x,
        y,
        BUTTON_W,
        BUTTON_H,
        "Save",
        true,
    );
    x += BUTTON_W + BUTTON_GAP;
    state.folder.save_as_btn = push_button(
        state.dialog,
        hinst,
        font,
        IDC_SAVE_AS_BUTTON,
        x,
        y,
        BUTTON_W,
        BUTTON_H,
        "Save As...",
        false,
    );
    x += BUTTON_W + BUTTON_GAP;
    state.folder.close_btn = push_button(
        state.dialog,
        hinst,
        font,
        IDC_CLOSE_BUTTON,
        x,
        y,
        BUTTON_W,
        BUTTON_H,
        "Close",
        false,
    );
}

// --- Control creation primitives -------------------------------

fn static_label(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    x: i32,
    y: i32,
    w: i32,
    text: &str,
) -> HWND {
    let wide = wide_terminated(text);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_LEFT),
            x,
            y,
            w,
            LABEL_H,
            Some(parent),
            None,
            Some(hinst),
            None,
        )
    }
    .unwrap_or(HWND::default());
    unsafe {
        apply_dialog_font(hwnd, font);
    }
    hwnd
}

fn edit_box(parent: HWND, hinst: HINSTANCE, font: HFONT, id: u16, x: i32, y: i32, w: i32) -> HWND {
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            None,
            WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            x,
            y,
            w,
            CTRL_H,
            Some(parent),
            Some(HMENU(id as usize as *mut c_void)),
            Some(hinst),
            None,
        )
    }
    .unwrap_or(HWND::default());
    unsafe {
        apply_dialog_font(hwnd, font);
    }
    hwnd
}

/// Multi-line edit box with vertical scrollbar, word-wrap, and
/// return-key handling. Used for the keyword-list edit on the
/// Keywords Lists tab (m3c) — long lists (Cisco IOS has hundreds
/// of entries) need to wrap and scroll, and Enter/Return must
/// insert a newline into the text rather than dismiss the dialog.
fn multi_line_edit(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    id: u16,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> HWND {
    // ES_MULTILINE: allow multiple lines
    // ES_AUTOVSCROLL: scroll vertically as content grows
    // ES_WANTRETURN: Return key inserts newline rather than
    //                triggering the dialog's default button
    // WS_VSCROLL: visible scrollbar
    const ES_MULTILINE: u32 = 0x0004;
    const ES_AUTOVSCROLL: u32 = 0x0040;
    const ES_WANTRETURN: u32 = 0x1000;

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            None,
            WS_CHILD
                | WS_VISIBLE
                | WS_BORDER
                | WS_TABSTOP
                | WS_VSCROLL
                | WINDOW_STYLE(ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as usize as *mut c_void)),
            Some(hinst),
            None,
        )
    }
    .unwrap_or(HWND::default());
    unsafe {
        apply_dialog_font(hwnd, font);
    }
    hwnd
}

/// Drop-down-list combobox (CBS_DROPDOWNLIST) — no free-text
/// entry, user picks from a fixed list. `h` is the combobox's
/// full height, which for a drop-down includes the extended
/// drop area (the height of the "expanded" list).
fn combo_box(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    id: u16,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> HWND {
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WC_COMBOBOX,
            None,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as usize as *mut c_void)),
            Some(hinst),
            None,
        )
    }
    .unwrap_or(HWND::default());
    unsafe {
        apply_dialog_font(hwnd, font);
    }
    hwnd
}

fn check_box(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    id: u16,
    x: i32,
    y: i32,
    w: i32,
    text: &str,
) -> HWND {
    let wide = wide_terminated(text);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            x,
            y,
            w,
            CTRL_H,
            Some(parent),
            Some(HMENU(id as usize as *mut c_void)),
            Some(hinst),
            None,
        )
    }
    .unwrap_or(HWND::default());
    unsafe {
        apply_dialog_font(hwnd, font);
        disable_visual_style(hwnd);
    }
    hwnd
}

fn radio_button(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    id: u16,
    x: i32,
    y: i32,
    w: i32,
    text: &str,
    first_in_group: bool,
) -> HWND {
    let wide = wide_terminated(text);
    let group_style = if first_in_group { WS_GROUP.0 } else { 0 };
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(wide.as_ptr()),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WINDOW_STYLE(group_style)
                | WINDOW_STYLE(BS_AUTORADIOBUTTON as u32),
            x,
            y,
            w,
            CTRL_H,
            Some(parent),
            Some(HMENU(id as usize as *mut c_void)),
            Some(hinst),
            None,
        )
    }
    .unwrap_or(HWND::default());
    unsafe {
        apply_dialog_font(hwnd, font);
        disable_visual_style(hwnd);
    }
    hwnd
}

fn group_box(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    text: &str,
) -> HWND {
    let wide = wide_terminated(text);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_GROUPBOX as u32),
            x,
            y,
            w,
            h,
            Some(parent),
            None,
            Some(hinst),
            None,
        )
    }
    .unwrap_or(HWND::default());
    unsafe {
        apply_dialog_font(hwnd, font);
        disable_visual_style(hwnd);
    }
    hwnd
}

fn push_button(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    id: u16,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    text: &str,
    default: bool,
) -> HWND {
    let wide = wide_terminated(text);
    let style_bits = if default {
        BS_DEFPUSHBUTTON
    } else {
        BS_PUSHBUTTON
    };
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(style_bits as u32),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as usize as *mut c_void)),
            Some(hinst),
            None,
        )
    }
    .unwrap_or(HWND::default());
    unsafe {
        apply_dialog_font(hwnd, font);
    }
    hwnd
}

// -------------------------------------------------------------
// Populate + read-back
// -------------------------------------------------------------

fn populate_folder_tab(state: &UdlEditorState) {
    set_edit_text(state.folder.name_edit, &state.definition.name);
    set_edit_text(
        state.folder.ext_edit,
        &state.definition.extensions.join(" "),
    );
    set_check(
        state.folder.case_ignored,
        state.definition.settings.case_ignored,
    );
    set_check(
        state.folder.allow_fold,
        state.definition.settings.allow_fold_of_comments,
    );
    set_check(
        state.folder.fold_compact,
        state.definition.settings.fold_compact,
    );
    let fplc = state.definition.settings.force_pure_lc.min(2) as usize;
    for (i, hwnd) in state.folder.fplc_radios.iter().enumerate() {
        set_check(*hwnd, i == fplc);
    }
    let dec = state.definition.settings.decimal_separator.min(2) as usize;
    for (i, hwnd) in state.folder.decimal_radios.iter().enumerate() {
        set_check(*hwnd, i == dec);
    }
}

/// Populate the Keywords Lists tab (m3c) from the current
/// definition. Sets the combobox selection to
/// `keywords.current_class` (0 on first open) and loads the
/// corresponding keyword list into the edit box + the prefix
/// checkbox from the class's prefix flag.
///
/// Wrapped in `with_modal_pump` so the wnd_proc reentry guard
/// suppresses any synchronous `EN_CHANGE` / `BN_CLICKED`
/// notifications the edit / combobox fire back at the parent
/// — otherwise the initial population would flip the "dirty"
/// bit on a freshly-loaded UDL.
fn populate_keywords_tab(state: &UdlEditorState) {
    with_modal_pump(|| {
        let idx = state.keywords.current_class.min(7);
        unsafe {
            SendMessageW(
                state.keywords.class_combo,
                CB_SETCURSEL,
                Some(WPARAM(idx)),
                None,
            );
        }
        set_edit_text(
            state.keywords.edit,
            &state.definition.keyword_lists.keywords[idx],
        );
        set_check(state.keywords.prefix_check, state.definition.prefix[idx]);
    });
}

/// Populate the Comment & Number tab (m3d) from the current
/// definition. Decodes the raw `comments` encoding into the
/// friendly fields and sets the 7 number sub-fields verbatim.
/// Wrapped in `with_modal_pump` so the synchronous `EN_CHANGE`
/// each `set_edit_text` fires doesn't flip the dirty bit on a
/// freshly-loaded UDL.
fn populate_comment_number_tab(state: &UdlEditorState) {
    with_modal_pump(|| {
        let decoded = decode_comments(&state.definition.keyword_lists.comments);
        set_edit_text(
            state.comment_number.line_marker,
            &decoded.line_markers.join(" "),
        );
        // The combobox sel index maps 1:1 to `LineCloseStyle`
        // discriminant order (see `build_comment_number_tab`).
        let sel = match decoded.line_close {
            LineCloseStyle::None => 0,
            LineCloseStyle::Eol => 1,
            LineCloseStyle::Eof => 2,
            LineCloseStyle::Both => 3,
        };
        unsafe {
            SendMessageW(
                state.comment_number.line_close_combo,
                CB_SETCURSEL,
                Some(WPARAM(sel)),
                None,
            );
        }
        set_edit_text(state.comment_number.block_open, &decoded.block_open);
        set_edit_text(state.comment_number.block_close, &decoded.block_close);

        set_edit_text(
            state.comment_number.num_prefix1,
            &state.definition.keyword_lists.numbers_prefix1,
        );
        set_edit_text(
            state.comment_number.num_prefix2,
            &state.definition.keyword_lists.numbers_prefix2,
        );
        set_edit_text(
            state.comment_number.num_extras1,
            &state.definition.keyword_lists.numbers_extras1,
        );
        set_edit_text(
            state.comment_number.num_extras2,
            &state.definition.keyword_lists.numbers_extras2,
        );
        set_edit_text(
            state.comment_number.num_suffix1,
            &state.definition.keyword_lists.numbers_suffix1,
        );
        set_edit_text(
            state.comment_number.num_suffix2,
            &state.definition.keyword_lists.numbers_suffix2,
        );
        set_edit_text(
            state.comment_number.num_range,
            &state.definition.keyword_lists.numbers_range,
        );
    });
}

/// Populate the Operators & Delimiters tab (m3e) from the current
/// definition. All three fields are verbatim string copies from
/// the model — the delimiter encoding round-trips as-is.
fn populate_operators_delimiters_tab(state: &UdlEditorState) {
    with_modal_pump(|| {
        set_edit_text(
            state.operators_delimiters.op1_edit,
            &state.definition.keyword_lists.operators1,
        );
        set_edit_text(
            state.operators_delimiters.op2_edit,
            &state.definition.keyword_lists.operators2,
        );
        set_edit_text(
            state.operators_delimiters.delims_edit,
            &state.definition.keyword_lists.delimiters,
        );
    });
}

/// Read the four comment-section edits + the line-close combo
/// and re-encode into `keyword_lists.comments`. Called on every
/// EN_CHANGE / CBN_SELCHANGE for a comment control so the model
/// stays in sync with the UI.
fn flush_comment_section_to_model(state: &mut UdlEditorState) {
    let markers_text = get_edit_text(state.comment_number.line_marker);
    let line_markers: Vec<String> = markers_text.split_whitespace().map(str::to_owned).collect();
    let sel = unsafe {
        SendMessageW(
            state.comment_number.line_close_combo,
            CB_GETCURSEL,
            None,
            None,
        )
        .0 as isize
    };
    let line_close = match sel {
        1 => LineCloseStyle::Eol,
        2 => LineCloseStyle::Eof,
        3 => LineCloseStyle::Both,
        _ => LineCloseStyle::None,
    };
    let block_open = get_edit_text(state.comment_number.block_open);
    let block_close = get_edit_text(state.comment_number.block_close);
    let decoded = DecodedComments {
        line_markers,
        line_close,
        block_open,
        block_close,
    };
    state.definition.keyword_lists.comments = encode_comments(&decoded);
}

fn set_edit_text(hwnd: HWND, text: &str) {
    let wide = wide_terminated(text);
    unsafe {
        SendMessageW(hwnd, WM_SETTEXT, None, Some(LPARAM(wide.as_ptr() as isize)));
    }
}

fn get_edit_text(hwnd: HWND) -> String {
    unsafe {
        let len = SendMessageW(hwnd, WM_GETTEXTLENGTH, None, None).0 as usize;
        if len == 0 {
            return String::new();
        }
        // Cap the read at a defensive upper bound so a hostile /
        // corrupt state can't OOM us. 65 536 UTF-16 code units
        // ≈ up to ~128 KiB of UTF-16 in memory — orders of
        // magnitude above any realistic UDL name / keyword-list /
        // extension entry the user is meant to type here, but
        // small enough that a runaway allocation is bounded.
        let cap = len.min(64 * 1024);
        let mut buf = vec![0u16; cap + 1];
        let got = SendMessageW(
            hwnd,
            WM_GETTEXT,
            Some(WPARAM(buf.len())),
            Some(LPARAM(buf.as_mut_ptr() as isize)),
        )
        .0 as usize;
        let end = got.min(buf.len().saturating_sub(1));
        String::from_utf16_lossy(&buf[..end])
    }
}

fn set_check(hwnd: HWND, checked: bool) {
    let state = if checked { BST_CHECKED } else { BST_UNCHECKED };
    unsafe {
        SendMessageW(hwnd, BM_SETCHECK, Some(WPARAM(state.0 as usize)), None);
    }
}

fn is_checked(hwnd: HWND) -> bool {
    let r = unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None) };
    r.0 as u32 == BST_CHECKED.0
}

// -------------------------------------------------------------
// Command dispatch
// -------------------------------------------------------------

/// Comment index-02 line-close style. UDLs express this as literal
/// `((EOL))` / `((EOF))` tokens which the parser recognises
/// specially — see [`codepp_udl::CommentRules::parse`].
///
/// **`Both`** is preserved (rather than collapsed to one of the
/// two) so a UDL that carries both variants round-trips losslessly;
/// the UI exposes the presence of both as a single "Both" combobox
/// selection.
///
/// **Runtime effect (Phase 4.6 m3d state).** The container-lexer
/// tokeniser in `crates/udl/src/tokenise.rs::match_line_comment`
/// currently ALWAYS terminates a line comment at `\n`, regardless
/// of which variant is selected here. `Eof` and `Both` are stored
/// and round-tripped correctly (Save writes them back verbatim to
/// the on-disk XML) but they do NOT yet change how live buffers
/// are highlighted. Wiring the tokeniser to consult
/// [`codepp_udl::CommentRules`]`::line_close` for EOF handling is
/// tracked as a follow-up in the udl crate; for now this enum is
/// faithful storage, not effective behaviour.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum LineCloseStyle {
    /// No index-02 token present. Line comment terminates at
    /// end-of-line (the tokeniser's default).
    #[default]
    None,
    /// `02((EOL))` — explicitly asks the parser to mark EOL as
    /// the close; matches the tokeniser's default behaviour.
    /// C, Python, Bash, Markdown all use this.
    Eol,
    /// `02((EOF))` — declared but not yet consulted by the
    /// tokeniser (see the type-level "Runtime effect" note).
    /// Cisco IOS declares this for `!` markers.
    Eof,
    /// Both `02((EOL))` and `02((EOF))` present in the source.
    Both,
}

/// A comment-encoding string (the `Comments` keyword-list value)
/// decomposed into the four friendly fields the m3d UI exposes.
/// Index-01 (line-continue) is deliberately omitted — no
/// real-world UDL uses it and N++'s own editor hides it too.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct DecodedComments {
    /// Index-00 markers. Cisco IOS carries two (`!` and `remark`);
    /// C carries one (`//`); markdown carries `#`.
    pub line_markers: Vec<String>,
    /// Index-02 style.
    pub line_close: LineCloseStyle,
    /// Index-03 block-comment opener.
    pub block_open: String,
    /// Index-04 block-comment closer.
    pub block_close: String,
}

/// Decode a `Comments` keyword-list value into structured fields.
/// Unknown / index-01 / index-≥5 tokens are silently dropped —
/// same tolerant-parsing discipline `codepp_udl::CommentRules::parse`
/// uses. Never panics — malformed tokens (< 2 bytes, non-digit
/// prefix, or a multi-byte codepoint straddling byte offset 2)
/// are silently skipped.
pub(crate) fn decode_comments(encoded: &str) -> DecodedComments {
    let mut out = DecodedComments::default();
    for tok in encoded.split_whitespace() {
        // A well-formed token is 2 ASCII digits + arbitrary
        // content. Guard both length AND char-boundary before
        // `split_at`, otherwise a hostile UDL with a multi-byte
        // codepoint in the FIRST byte of a token (e.g.
        // "日remark", "0é...") would panic here — real DoS
        // vector because `decode_comments` runs on every UDL
        // load in the "Define your language…" flow, and the
        // in-process `panic = "abort"` release profile takes
        // down the whole editor.
        let Some(prefix) = tok.get(..2) else { continue };
        let Some(content) = tok.get(2..) else {
            continue;
        };
        let Ok(idx) = prefix.parse::<u8>() else {
            continue;
        };
        match idx {
            0 if !content.is_empty() => out.line_markers.push(content.to_owned()),
            2 => match content {
                "((EOL))" => {
                    out.line_close = match out.line_close {
                        LineCloseStyle::None => LineCloseStyle::Eol,
                        LineCloseStyle::Eof | LineCloseStyle::Both => LineCloseStyle::Both,
                        LineCloseStyle::Eol => LineCloseStyle::Eol,
                    };
                }
                "((EOF))" => {
                    out.line_close = match out.line_close {
                        LineCloseStyle::None => LineCloseStyle::Eof,
                        LineCloseStyle::Eol | LineCloseStyle::Both => LineCloseStyle::Both,
                        LineCloseStyle::Eof => LineCloseStyle::Eof,
                    };
                }
                _ => {}
            },
            // Last-wins on indices 3/4, matching
            // `codepp_udl::CommentRules::parse`'s discipline
            // (unconditional overwrite). Real UDLs never declare
            // duplicate block markers, but a hand-edited or
            // hostile file that does must decode to the same
            // shape here as it does through the runtime parser,
            // otherwise an edit-and-resave loses whichever the
            // runtime was actually using.
            3 => content.clone_into(&mut out.block_open),
            4 => content.clone_into(&mut out.block_close),
            _ => {}
        }
    }
    out
}

/// Encode a [`DecodedComments`] back to the space-separated
/// `NN<content>` string the tokeniser consumes. Symmetric with
/// [`decode_comments`] — `encode(decode(x))` normalises `x`
/// (drops index-01 and any garbage; collapses duplicates on
/// indices 3/4) but round-trips clean data losslessly.
pub(crate) fn encode_comments(d: &DecodedComments) -> String {
    let mut parts: Vec<String> = Vec::new();
    for marker in &d.line_markers {
        if !marker.is_empty() {
            parts.push(format!("00{marker}"));
        }
    }
    match d.line_close {
        LineCloseStyle::None => {}
        LineCloseStyle::Eol => parts.push("02((EOL))".to_owned()),
        LineCloseStyle::Eof => parts.push("02((EOF))".to_owned()),
        LineCloseStyle::Both => {
            parts.push("02((EOL))".to_owned());
            parts.push("02((EOF))".to_owned());
        }
    }
    if !d.block_open.is_empty() {
        parts.push(format!("03{}", d.block_open));
    }
    if !d.block_close.is_empty() {
        parts.push(format!("04{}", d.block_close));
    }
    parts.join(" ")
}

/// Pure swap-dance for the Keywords Lists tab's class-combobox
/// selection change. Flushes the caller-provided `flushed_text`
/// (what's currently in the edit box) into `def.keyword_lists.
/// keywords[prev]`, then returns `(new_text, new_prefix)` for the
/// caller to load into the edit + checkbox at `new`. Assumes `new
/// < 8` — callers bounds-check the combobox selection first (see
/// the `IDC_KW_CLASS_COMBO` arm in `handle_command`).
///
/// Kept as a free function so the unit tests exercise the actual
/// wnd_proc logic rather than reimplementing it inline. Doesn't
/// touch any HWND state; the wnd_proc arm handles that side.
fn swap_keyword_class(
    def: &mut UdlDefinition,
    prev: usize,
    flushed_text: String,
    new: usize,
) -> (String, bool) {
    debug_assert!(prev < 8 && new < 8, "class indices must be 0..=7");
    def.keyword_lists.keywords[prev] = flushed_text;
    (def.keyword_lists.keywords[new].clone(), def.prefix[new])
}

fn handle_command(state: &mut UdlEditorState, wparam: WPARAM, _lparam: LPARAM) {
    let id = (wparam.0 & 0xFFFF) as u16;
    let notify_code = ((wparam.0 >> 16) & 0xFFFF) as u16;
    match id {
        IDC_NAME_EDIT if notify_code == EN_CHANGE as u16 => {
            state.definition.name = get_edit_text(state.folder.name_edit);
            state.dirty = true;
        }
        IDC_EXT_EDIT if notify_code == EN_CHANGE as u16 => {
            state.definition.extensions = get_edit_text(state.folder.ext_edit)
                .split_whitespace()
                .map(str::to_ascii_lowercase)
                .collect();
            state.dirty = true;
        }
        IDC_CASE_IGNORED => {
            state.definition.settings.case_ignored = is_checked(state.folder.case_ignored);
            state.dirty = true;
        }
        IDC_ALLOW_FOLD => {
            state.definition.settings.allow_fold_of_comments = is_checked(state.folder.allow_fold);
            state.dirty = true;
        }
        IDC_FOLD_COMPACT => {
            state.definition.settings.fold_compact = is_checked(state.folder.fold_compact);
            state.dirty = true;
        }
        IDC_FPLC_RADIO_0 | IDC_FPLC_RADIO_1 | IDC_FPLC_RADIO_2 => {
            let idx = (id - IDC_FPLC_RADIO_0) as u8;
            state.definition.settings.force_pure_lc = idx;
            state.dirty = true;
        }
        IDC_DECIMAL_RADIO_0 | IDC_DECIMAL_RADIO_1 | IDC_DECIMAL_RADIO_2 => {
            let idx = (id - IDC_DECIMAL_RADIO_0) as u8;
            state.definition.settings.decimal_separator = idx;
            state.dirty = true;
        }
        // --- Keywords Lists tab (Phase 4.6 m3c) ---
        IDC_KW_CLASS_COMBO if notify_code == CBN_SELCHANGE as u16 => {
            let prev = state.keywords.current_class;
            let flushed = get_edit_text(state.keywords.edit);
            let new_class_raw = unsafe {
                SendMessageW(state.keywords.class_combo, CB_GETCURSEL, None, None).0 as isize
            };
            if new_class_raw < 0
                || new_class_raw as usize >= state.definition.keyword_lists.keywords.len()
            {
                return;
            }
            let new_class = new_class_raw as usize;
            // All the pure logic lives in `swap_keyword_class`,
            // which the tests exercise directly. The HWND-coupled
            // part (writing the new text back to the edit + the
            // prefix flag to the checkbox) stays here.
            let (new_text, new_prefix) =
                swap_keyword_class(&mut state.definition, prev, flushed, new_class);
            state.keywords.current_class = new_class;
            with_modal_pump(|| {
                set_edit_text(state.keywords.edit, &new_text);
                set_check(state.keywords.prefix_check, new_prefix);
            });
        }
        IDC_KW_EDIT if notify_code == EN_CHANGE as u16 => {
            let idx = state.keywords.current_class;
            state.definition.keyword_lists.keywords[idx] = get_edit_text(state.keywords.edit);
            state.dirty = true;
        }
        IDC_KW_PREFIX_CHECK => {
            let idx = state.keywords.current_class;
            state.definition.prefix[idx] = is_checked(state.keywords.prefix_check);
            state.dirty = true;
        }
        // --- Comment & Number tab (Phase 4.6 m3d) ---
        // Comment section: any change to a comment field
        // re-encodes ALL four fields into `comments`.
        IDC_CM_LINE_MARKER | IDC_CM_BLOCK_OPEN | IDC_CM_BLOCK_CLOSE
            if notify_code == EN_CHANGE as u16 =>
        {
            flush_comment_section_to_model(state);
            state.dirty = true;
        }
        IDC_CM_LINE_CLOSE_COMBO if notify_code == CBN_SELCHANGE as u16 => {
            flush_comment_section_to_model(state);
            state.dirty = true;
        }
        // Number section: each field mirrors one keyword-list
        // slot verbatim; no encoding.
        IDC_NUM_PREFIX1 if notify_code == EN_CHANGE as u16 => {
            state.definition.keyword_lists.numbers_prefix1 =
                get_edit_text(state.comment_number.num_prefix1);
            state.dirty = true;
        }
        IDC_NUM_PREFIX2 if notify_code == EN_CHANGE as u16 => {
            state.definition.keyword_lists.numbers_prefix2 =
                get_edit_text(state.comment_number.num_prefix2);
            state.dirty = true;
        }
        IDC_NUM_EXTRAS1 if notify_code == EN_CHANGE as u16 => {
            state.definition.keyword_lists.numbers_extras1 =
                get_edit_text(state.comment_number.num_extras1);
            state.dirty = true;
        }
        IDC_NUM_EXTRAS2 if notify_code == EN_CHANGE as u16 => {
            state.definition.keyword_lists.numbers_extras2 =
                get_edit_text(state.comment_number.num_extras2);
            state.dirty = true;
        }
        IDC_NUM_SUFFIX1 if notify_code == EN_CHANGE as u16 => {
            state.definition.keyword_lists.numbers_suffix1 =
                get_edit_text(state.comment_number.num_suffix1);
            state.dirty = true;
        }
        IDC_NUM_SUFFIX2 if notify_code == EN_CHANGE as u16 => {
            state.definition.keyword_lists.numbers_suffix2 =
                get_edit_text(state.comment_number.num_suffix2);
            state.dirty = true;
        }
        IDC_NUM_RANGE if notify_code == EN_CHANGE as u16 => {
            state.definition.keyword_lists.numbers_range =
                get_edit_text(state.comment_number.num_range);
            state.dirty = true;
        }
        // --- Operators & Delimiters tab (Phase 4.6 m3e) ---
        // All three edits are verbatim mirrors — no encoding
        // interpretation on write, just a plain string copy.
        IDC_OP1_EDIT if notify_code == EN_CHANGE as u16 => {
            state.definition.keyword_lists.operators1 =
                get_edit_text(state.operators_delimiters.op1_edit);
            state.dirty = true;
        }
        IDC_OP2_EDIT if notify_code == EN_CHANGE as u16 => {
            state.definition.keyword_lists.operators2 =
                get_edit_text(state.operators_delimiters.op2_edit);
            state.dirty = true;
        }
        IDC_DELIMS_EDIT if notify_code == EN_CHANGE as u16 => {
            state.definition.keyword_lists.delimiters =
                get_edit_text(state.operators_delimiters.delims_edit);
            state.dirty = true;
        }
        IDC_SAVE_BUTTON => save_action(state, false),
        IDC_SAVE_AS_BUTTON => save_action(state, true),
        IDC_CLOSE_BUTTON => unsafe {
            let _ = PostMessageW(Some(state.dialog), WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        _ => {}
    }
}

fn switch_tab(state: &mut UdlEditorState, new_tab: usize) {
    if new_tab == state.current_tab {
        return;
    }
    for (i, page) in state.tab_pages.iter().enumerate() {
        unsafe {
            let _ = ShowWindow(*page, if i == new_tab { SW_SHOW } else { SW_HIDE });
        }
    }
    state.current_tab = new_tab;
}

// -------------------------------------------------------------
// Save / Save As / Close
// -------------------------------------------------------------

fn save_action(state: &mut UdlEditorState, force_prompt: bool) {
    // Determine the target path.
    let target = if force_prompt || state.source_path.is_none() {
        // `GetSaveFileNameW` runs a nested message pump; guard
        // wnd_proc re-entries so the outer `&mut UdlEditorState`
        // borrow is the only live borrow while the dialog is open.
        let owner = state.dialog;
        let default_name = state.definition.name.clone();
        let picked = with_modal_pump(|| prompt_udl_save_path(owner, &default_name));
        let Some(p) = picked else { return };
        p
    } else {
        state.source_path.clone().unwrap()
    };

    // Warn — but do not reject — if the user saved outside
    // `<config_dir>/userDefineLangs/`. The Language menu is
    // populated exclusively from that directory (via
    // `UdlRegistry::scan_dir` on the file-watched location), so
    // an out-of-directory save silently drops the UDL from the
    // menu. Mirrors N++'s "we let you save anywhere, but the
    // menu will only refresh if it's in the right place" UX.
    if let Some(dir) = codepp_platform::user_define_langs_dir() {
        if !path_is_under(&target, &dir) {
            let owner = state.dialog;
            let msg = format!(
                "The UDL was saved to:\n\n  {}\n\nThis is outside the UDL directory:\n\n  {}\n\n\
                 The UDL will NOT appear in the Language menu until it's copied there.",
                target.display(),
                dir.display(),
            );
            with_modal_pump(|| show_warning(owner, "UDL saved outside menu directory", &msg));
        }
    }

    // Atomic write: temp file + rename. The `save_to_file`
    // primitive documents itself as caller-responsible for
    // atomicity (see `codepp_udl::UdlDefinition::save_to_file`);
    // `save_action` is the "editor" caller that layers the temp
    // + rename on top. A crash / disk-full mid-write leaves the
    // temp file behind but does NOT corrupt any pre-existing
    // `<target>` — matching what `shell::fif`'s replace-in-files
    // pass does for the same reason (DESIGN.md §7.4).
    if let Err(err) = save_atomically(&state.definition, &target) {
        show_error(state.dialog, "Save failed", &format!("{err}"));
        return;
    }
    // Persist the save target on the state so subsequent plain-
    // Save clicks write to the same file.
    state.source_path = Some(target);
    state.definition.source_path = state.source_path.clone();
    state.dirty = false;

    // Ask the main window to re-scan the registry so the new UDL
    // appears in the Language menu.
    unsafe {
        let _ = PostMessageW(
            Some(state.main_hwnd),
            WM_APP_UDL_REFRESH,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

/// Write `udl` to `path` atomically via temp-file + rename. If
/// the write to the temp file fails, no rename happens and the
/// pre-existing `path` (if any) is untouched. If the rename
/// fails, the temp file is best-effort-cleaned; the pre-existing
/// `path` is still untouched.
///
/// **Windows atomicity notes.** `std::fs::rename` on Windows
/// dispatches to `MoveFileExW` which is atomic *only* if the
/// source and destination are on the same volume — which they
/// always are here because the temp file is a sibling of the
/// target (constructed by appending a `.tmp-<pid>` suffix). This
/// mirrors the `shell::fif` in-place-replace convention.
fn save_atomically(udl: &UdlDefinition, path: &Path) -> Result<(), codepp_udl::UdlError> {
    let temp = temp_path_for(path);
    udl.save_to_file(&temp)?;
    if let Err(err) = std::fs::rename(&temp, path) {
        // Best-effort cleanup so we don't litter the config dir
        // with `.tmp` files on rename failure. Errors ignored —
        // the primary failure (rename) is what the caller sees.
        let _ = std::fs::remove_file(&temp);
        return Err(codepp_udl::UdlError::Io {
            path: path.to_path_buf(),
            source: err,
        });
    }
    Ok(())
}

/// Build the `.tmp-<pid>` sibling path used by [`save_atomically`].
/// Extracted so a test can pin the naming convention.
pub(crate) fn temp_path_for(path: &Path) -> PathBuf {
    let mut buf = path.as_os_str().to_owned();
    buf.push(format!(".tmp-{}", std::process::id()));
    PathBuf::from(buf)
}

/// Return true iff `path` is under (or equal to) `dir`, using a
/// canonicalize-and-prefix check when both paths canonicalize. If
/// either side fails to canonicalize (e.g. `path` doesn't exist
/// yet — which is exactly the "user just typed a new filename"
/// case), fall back to a byte-level prefix check against the
/// user-typed path. This matches the containment discipline
/// [`codepp_udl::UdlRegistry::scan_dir`] applies on the read side.
pub(crate) fn path_is_under(path: &Path, dir: &Path) -> bool {
    // Canonicalize `dir` first — it should always exist (the
    // startup path creates it). If it doesn't canonicalize, treat
    // that as "we can't reason about containment; accept the
    // path" — the caller will still get the "your UDL is
    // outside" warning when the containment check fails later.
    let dir_canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    // For `path`, prefer the canonical form when available. When
    // the user is saving a brand-new file, `path` doesn't yet
    // exist, so `canonicalize` fails; fall back to the parent's
    // canonical form + the file name.
    let path_canon = path.canonicalize().unwrap_or_else(|_| {
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            let mut p = parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf());
            p.push(name);
            p
        } else {
            path.to_path_buf()
        }
    });
    path_canon.starts_with(&dir_canon)
}

fn show_warning(owner: HWND, title: &str, msg: &str) {
    let t = wide_terminated(title);
    let m = wide_terminated(msg);
    unsafe {
        let _ = MessageBoxW(
            Some(owner),
            PCWSTR(m.as_ptr()),
            PCWSTR(t.as_ptr()),
            MB_ICONWARNING | MB_OK,
        );
    }
}

fn confirm_discard_if_dirty(state: &UdlEditorState) -> bool {
    if !state.dirty {
        return true;
    }
    let owner = state.dialog;
    let title = wide_terminated("Unsaved changes");
    let msg = wide_terminated("The UDL has unsaved changes. Discard them and close?");
    // `MessageBoxW` runs a nested pump; guard wnd_proc against
    // reentering our `&UdlEditorState` borrow scope.
    let r = with_modal_pump(|| unsafe {
        MessageBoxW(
            Some(owner),
            PCWSTR(msg.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNOCANCEL | MB_ICONWARNING,
        )
    });
    r == IDYES
}

fn show_error(owner: HWND, title: &str, msg: &str) {
    let t = wide_terminated(title);
    let m = wide_terminated(msg);
    with_modal_pump(|| unsafe {
        let _ = MessageBoxW(
            Some(owner),
            PCWSTR(m.as_ptr()),
            PCWSTR(t.as_ptr()),
            MB_ICONERROR | MB_OK,
        );
    });
}

/// Prompt the user for a `.udl.xml` filename under the config
/// directory. Returns the chosen full path.
///
/// Defaults to `<config_dir>/userDefineLangs/<sanitized-name>.udl.xml`
/// so the "New UDL" flow lands the file where the startup scan
/// will find it.
fn prompt_udl_save_path(owner: HWND, default_name: &str) -> Option<PathBuf> {
    use windows::Win32::UI::Controls::Dialogs::{
        GetSaveFileNameW, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT,
        OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    let filter: Vec<u16> = "UDL files (*.udl.xml)\0*.udl.xml\0All files (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let default_file = sanitize_default_filename(default_name);
    let mut path_buf: Vec<u16> = default_file
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    path_buf.resize(1024, 0);

    let default_dir_wide = codepp_platform::user_define_langs_dir().map(|p| {
        p.to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    });

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(path_buf.as_mut_ptr()),
        nMaxFile: path_buf.len() as u32,
        lpstrInitialDir: default_dir_wide
            .as_ref()
            .map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
        lpstrDefExt: w!("udl.xml"),
        nFilterIndex: 1,
        Flags: OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY | OFN_NOCHANGEDIR,
        ..Default::default()
    };

    let ok = unsafe { GetSaveFileNameW(&raw mut ofn) }.as_bool();
    if !ok {
        return None;
    }
    let nul = path_buf
        .iter()
        .position(|&u| u == 0)
        .unwrap_or(path_buf.len());
    if nul == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&path_buf[..nul])))
}

/// Produce a default filename from a UDL name. Strips characters
/// that are not filesystem-safe on Windows and caps the length.
/// Used only as a Save-dialog default; the user can edit before
/// confirming.
pub(crate) fn sanitize_default_filename(name: &str) -> String {
    const DISALLOWED: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let base: String = name
        .chars()
        .filter(|c| !DISALLOWED.contains(c) && !c.is_control())
        .collect();
    let base = base.trim();
    let base = if base.is_empty() { "untitled" } else { base };
    // Cap raw base at 64 codepoints to leave headroom for the
    // ".udl.xml" suffix under Windows' 260-char limit even in
    // deep directory paths.
    let capped: String = base.chars().take(64).collect();
    format!("{capped}.udl.xml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_default_filename_strips_disallowed() {
        assert_eq!(sanitize_default_filename("Markdown"), "Markdown.udl.xml");
        // `/` chars are stripped; the two dots between them survive.
        assert_eq!(
            sanitize_default_filename("path/../evil"),
            "path..evil.udl.xml"
        );
        assert_eq!(sanitize_default_filename(""), "untitled.udl.xml");
        assert_eq!(sanitize_default_filename("<>:\""), "untitled.udl.xml");
    }

    #[test]
    fn sanitize_default_filename_strips_control_chars() {
        // C0 controls (NUL, ANSI-escape) that would either break
        // Win32 CreateFileW or produce visually-broken filenames
        // must be stripped.
        assert_eq!(
            sanitize_default_filename("hello\u{00}world\u{1B}[31m"),
            "helloworld[31m.udl.xml"
        );
    }

    #[test]
    fn sanitize_default_filename_caps_length() {
        let long = "a".repeat(200);
        let result = sanitize_default_filename(&long);
        // 64 caps the base; the ".udl.xml" suffix is 8 more.
        assert_eq!(result.len(), 64 + 8);
    }

    #[test]
    fn default_new_udl_has_24_style_slots() {
        // Pin: the New UDL flow must produce all 24 style slots
        // matching N++ convention, so the serialised output is a
        // well-formed UDL that reloads cleanly.
        let udl = default_new_udl();
        assert_eq!(udl.styles.len(), 24);
        assert_eq!(udl.styles[0].name, "DEFAULT");
        assert!(udl.styles.iter().any(|s| s.name == "OPERATORS"));
        assert!(udl.styles.iter().any(|s| s.name == "DELIMITERS8"));
    }

    #[test]
    fn temp_path_appends_tmp_suffix_next_to_target() {
        // Pin the atomic-write naming convention. The temp file
        // must be a same-directory sibling of the target so
        // `std::fs::rename` on Windows dispatches to the
        // same-volume atomic-rename path.
        let target = std::path::Path::new("C:/config/userDefineLangs/foo.udl.xml");
        let temp = temp_path_for(target);
        assert_eq!(temp.parent(), target.parent(), "temp must be a sibling");
        let temp_name = temp.file_name().unwrap().to_string_lossy();
        assert!(
            temp_name.starts_with("foo.udl.xml.tmp-"),
            "temp filename should be `<target>.tmp-<pid>`; got {temp_name}"
        );
    }

    #[test]
    fn path_is_under_recognises_containment_via_prefix() {
        // Non-canonicalising path check: matches whenever `path`
        // is byte-prefixed by `dir` (after both fall through the
        // canonicalize-or-passthrough). Doesn't touch the
        // filesystem here — the canonicalize failures fall
        // through to the raw-path comparison.
        let dir = std::path::Path::new("Z:/nonexistent/userDefineLangs");
        let inside = std::path::Path::new("Z:/nonexistent/userDefineLangs/foo.udl.xml");
        let outside = std::path::Path::new("Z:/nonexistent/elsewhere/foo.udl.xml");
        assert!(
            path_is_under(inside, dir),
            "inside path must be recognised as contained"
        );
        assert!(
            !path_is_under(outside, dir),
            "outside path must NOT be recognised as contained"
        );
    }

    #[test]
    fn swap_keyword_class_flushes_prev_and_returns_new_state() {
        // Actual regression pin for the `IDC_KW_CLASS_COMBO` swap
        // dance — exercises the same `swap_keyword_class` the
        // wnd_proc arm calls. Would have caught (and would still
        // catch) a re-ordering that either drops the caller's
        // flushed text or reads the new class's data BEFORE
        // writing the previous class's.
        let mut udl = default_new_udl();
        udl.keyword_lists.keywords[0] = "orig_kw1".to_owned();
        udl.keyword_lists.keywords[1] = "orig_kw2".to_owned();
        udl.prefix[1] = true;

        // User was in Keywords 1, typed "typed_new" into the
        // edit box, then picked Keywords 2 from the combobox.
        let (new_text, new_prefix) = swap_keyword_class(&mut udl, 0, "typed_new".to_owned(), 1);

        assert_eq!(new_text, "orig_kw2", "load new class's saved keywords");
        assert!(new_prefix, "load new class's saved prefix flag");
        assert_eq!(
            udl.keyword_lists.keywords[0], "typed_new",
            "prev class's edit content must be flushed to the model"
        );
        assert_eq!(
            udl.keyword_lists.keywords[1], "orig_kw2",
            "new class's slot must NOT be overwritten by the flush"
        );
    }

    #[test]
    fn swap_keyword_class_self_swap_is_idempotent() {
        // Pin the degenerate case: swapping to the same class the
        // user is already on (e.g. the CBN_SELCHANGE fires with
        // an unchanged selection — Win32 combobox quirk). The
        // flushed text becomes both the write AND the read, and
        // `new_text` matches what we just flushed.
        let mut udl = default_new_udl();
        udl.keyword_lists.keywords[3] = "before".to_owned();
        let (new_text, _prefix) = swap_keyword_class(&mut udl, 3, "typed_new".to_owned(), 3);
        assert_eq!(new_text, "typed_new");
        assert_eq!(udl.keyword_lists.keywords[3], "typed_new");
    }

    #[test]
    fn swap_keyword_class_all_indices_isolated() {
        // Off-by-one pin: swap at index N must ONLY touch
        // keywords[prev] and read keywords[new]/prefix[new];
        // every other slot must be untouched. Iterates every
        // (prev, new) pair over 0..=7 × 0..=7 to catch any
        // stray `.iter_mut()` bug.
        for prev in 0..8 {
            for new in 0..8 {
                let mut udl = default_new_udl();
                for i in 0..8 {
                    udl.keyword_lists.keywords[i] = format!("orig_{i}");
                    udl.prefix[i] = i % 2 == 0;
                }
                let _ = swap_keyword_class(&mut udl, prev, "FLUSHED".to_owned(), new);
                for i in 0..8 {
                    let expected = if i == prev {
                        "FLUSHED".to_owned()
                    } else {
                        format!("orig_{i}")
                    };
                    assert_eq!(
                        udl.keyword_lists.keywords[i], expected,
                        "prev={prev} new={new}: keywords[{i}] must be untouched \
                         except at prev"
                    );
                    assert_eq!(
                        udl.prefix[i],
                        i % 2 == 0,
                        "prev={prev} new={new}: prefix[{i}] must be untouched"
                    );
                }
            }
        }
    }

    #[test]
    fn decode_comments_recognises_c_style() {
        // `00// 01\ 02((EOL)) 03/* 04*/` = C-style comments.
        let decoded = decode_comments("00// 01\\ 02((EOL)) 03/* 04*/");
        assert_eq!(decoded.line_markers, vec!["//".to_owned()]);
        assert_eq!(decoded.line_close, LineCloseStyle::Eol);
        assert_eq!(decoded.block_open, "/*");
        assert_eq!(decoded.block_close, "*/");
    }

    #[test]
    fn decode_comments_recognises_cisco_ios_multi_marker() {
        // Cisco IOS UDL uses both `!` and `remark` as line
        // markers, and closes at EOF (line comment continues
        // until end of file when unclosed).
        let decoded = decode_comments("00! 00remark 01 02((EOF)) 03 04");
        assert_eq!(
            decoded.line_markers,
            vec!["!".to_owned(), "remark".to_owned()]
        );
        assert_eq!(decoded.line_close, LineCloseStyle::Eof);
        assert_eq!(decoded.block_open, "");
        assert_eq!(decoded.block_close, "");
    }

    #[test]
    fn decode_comments_recognises_markdown_fixture() {
        // Markdown preinstalled fixture uses `#` line marker + EOL
        // close + `<!--` / `-->` block markers.
        let decoded = decode_comments("00# 01 02((EOL)) 03<!-- 04-->");
        assert_eq!(decoded.line_markers, vec!["#".to_owned()]);
        assert_eq!(decoded.line_close, LineCloseStyle::Eol);
        assert_eq!(decoded.block_open, "<!--");
        assert_eq!(decoded.block_close, "-->");
    }

    #[test]
    fn decode_comments_recognises_both_line_close() {
        let decoded = decode_comments("00# 02((EOL)) 02((EOF))");
        assert_eq!(decoded.line_close, LineCloseStyle::Both);
    }

    #[test]
    fn decode_comments_empty_input_yields_default() {
        let decoded = decode_comments("");
        assert_eq!(decoded, DecodedComments::default());
    }

    #[test]
    fn decode_comments_never_panics_on_non_char_boundary_input() {
        // Regression pin for the m3d critical audit finding:
        // `tok.split_at(2)` used to panic when byte offset 2 fell
        // inside a multi-byte codepoint. A UDL whose comments
        // value contained non-ASCII text was a one-file DoS on
        // the whole editor because `decode_comments` runs on
        // every "Define your language…" open and the release
        // profile is `panic = "abort"`. Fix uses `tok.get(..2)`
        // / `tok.get(2..)` (returns `None` on non-char-boundary
        // rather than panicking).
        //
        // Exercise every codepoint shape at every position:
        //  - 3-byte codepoint at offset 0 (`"日x"`)
        //  - 2-byte codepoint straddling offset 1-2 (`"0éx"`)
        //  - 4-byte codepoint at offset 0 (astral, `"🎉x"`)
        //  - Isolated single-byte tokens ("a", "01", "1")
        // None of these panic; malformed tokens are dropped.
        let _ = decode_comments("日x");
        let _ = decode_comments("0éx");
        let _ = decode_comments("🎉x");
        let _ = decode_comments("a");
        let _ = decode_comments("01");
        let _ = decode_comments("1");
        // A well-formed marker adjacent to a hostile token: the
        // hostile one drops, the well-formed one survives.
        let decoded = decode_comments("日hostile 00# 0éalso 02((EOL))");
        assert_eq!(decoded.line_markers, vec!["#".to_owned()]);
        assert_eq!(decoded.line_close, LineCloseStyle::Eol);
    }

    #[test]
    fn decode_comments_ignores_index_01_and_garbage() {
        // Index-01 (line-continue) and unknown indices ≥5 are
        // silently dropped — matches the parser's tolerance.
        let decoded = decode_comments("00# 01\\ 05junk 99garbage");
        assert_eq!(decoded.line_markers, vec!["#".to_owned()]);
        assert_eq!(decoded.block_open, "");
        assert_eq!(decoded.block_close, "");
    }

    #[test]
    fn encode_comments_round_trips_c_style() {
        let decoded = DecodedComments {
            line_markers: vec!["//".to_owned()],
            line_close: LineCloseStyle::Eol,
            block_open: "/*".to_owned(),
            block_close: "*/".to_owned(),
        };
        let encoded = encode_comments(&decoded);
        assert_eq!(encoded, "00// 02((EOL)) 03/* 04*/");
        // Symmetry: re-decode reproduces the input.
        assert_eq!(decode_comments(&encoded), decoded);
    }

    #[test]
    fn encode_comments_round_trips_cisco_multi_marker() {
        let decoded = DecodedComments {
            line_markers: vec!["!".to_owned(), "remark".to_owned()],
            line_close: LineCloseStyle::Eof,
            block_open: String::new(),
            block_close: String::new(),
        };
        let encoded = encode_comments(&decoded);
        assert_eq!(encoded, "00! 00remark 02((EOF))");
        assert_eq!(decode_comments(&encoded), decoded);
    }

    #[test]
    fn encode_comments_omits_empty_markers() {
        // Empty markers must not be written back (a `00` prefix
        // with no content would round-trip as a "marker of
        // zero length" which the tokeniser silently discards).
        let decoded = DecodedComments {
            line_markers: vec![String::new(), "#".to_owned(), String::new()],
            line_close: LineCloseStyle::None,
            block_open: String::new(),
            block_close: String::new(),
        };
        let encoded = encode_comments(&decoded);
        assert_eq!(encoded, "00#");
    }

    #[test]
    fn encode_comments_empty_decoded_yields_empty_string() {
        let encoded = encode_comments(&DecodedComments::default());
        assert!(encoded.is_empty());
    }

    #[test]
    fn round_trip_matches_codepp_udl_parser() {
        // End-to-end pin: the encoded output must be readable by
        // the actual `codepp_udl::CommentRules::parse` used at
        // load time. Guards against a divergence between our
        // encoder and the tokeniser it feeds — the whole point
        // of the m3d flow is that Save → reload works.
        use codepp_udl::Sequence;
        let decoded = DecodedComments {
            line_markers: vec!["//".to_owned()],
            line_close: LineCloseStyle::Eol,
            block_open: "/*".to_owned(),
            block_close: "*/".to_owned(),
        };
        let encoded = encode_comments(&decoded);
        let parsed = codepp_udl::CommentRules::parse(&encoded);
        assert_eq!(parsed.line_open, vec![Sequence::Literal("//".to_owned())]);
        assert_eq!(parsed.block_open, Sequence::Literal("/*".to_owned()));
        assert_eq!(parsed.block_close, Sequence::Literal("*/".to_owned()));
    }

    #[test]
    fn encode_then_decode_round_trips_both_line_close() {
        // Symmetry pin for the `Both` promotion matrix — the
        // most intricate logic path in `decode_comments`. The
        // other 3 variants get symmetric coverage from the C-
        // style / Cisco tests; `Both` was previously only
        // covered on the decode direction.
        let decoded = DecodedComments {
            line_markers: vec!["#".to_owned()],
            line_close: LineCloseStyle::Both,
            block_open: String::new(),
            block_close: String::new(),
        };
        let encoded = encode_comments(&decoded);
        assert_eq!(encoded, "00# 02((EOL)) 02((EOF))");
        assert_eq!(decode_comments(&encoded), decoded);
    }

    #[test]
    fn decode_comments_index_3_and_4_are_last_wins() {
        // Regression pin for the m3d re-review warning:
        // `codepp_udl::CommentRules::parse` treats duplicate
        // index-3/4 tokens as last-wins (unconditional
        // overwrite). Our decoder must match — otherwise a
        // hand-edited UDL with duplicates would round-trip to
        // a different value through the m3d editor than what
        // the runtime tokeniser actually uses.
        let decoded = decode_comments("03first 03second 04close_first 04close_last");
        assert_eq!(decoded.block_open, "second");
        assert_eq!(decoded.block_close, "close_last");
    }

    #[test]
    fn default_new_udl_round_trips_through_serializer() {
        // Regression pin against m3a: the "New UDL" starting
        // point must round-trip through `to_xml_string` and
        // `parse` back to an equal value. Anything less and Save
        // on a fresh dialog would produce an unparseable file.
        let udl = default_new_udl();
        let xml = udl.to_xml_string();
        let mut reparsed =
            codepp_udl::UdlDefinition::parse(&xml).expect("New UDL default must round-trip");
        // parse() populates source_path from `from_file` only;
        // both sides should be None here.
        reparsed.source_path = None;
        let mut original = udl;
        original.source_path = None;
        assert_eq!(original, reparsed);
    }
}
