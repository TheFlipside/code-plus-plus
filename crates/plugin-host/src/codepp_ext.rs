//! Code++-specific plugin host messages — **not** part of the
//! Notepad++ ABI.
//!
//! Notepad++'s message set (`NPPM_*` / `NPPN_*`, mirrored in
//! [`crate::ffi`] and [`crate::dispatch`]) has no way for a plugin to
//! ask the host to pop a native "Save As" dialog or to put arbitrary
//! clipboard formats on the system clipboard. Upstream plugins that
//! need those (e.g. `NppExport`) call the Win32 API directly, which is
//! exactly what makes them non-portable.
//!
//! Code++ closes that gap with two host-provided capabilities so a
//! plugin stays platform-agnostic: it hands the host semantic bytes,
//! and each backend (Win32 / GTK / Cocoa) does the platform-specific
//! dialog + file write and the platform-specific clipboard packaging.
//! The in-tree `cppexport` plugin is the first (and today only) user.
//!
//! These messages live in a dedicated numeric band well clear of the
//! `NPPMSG` (`WM_USER + 1000`) and `RUNCOMMAND_USER` (`WM_USER + 3000`)
//! ranges so they can never collide with a present or future real
//! Notepad++ message. They are additive: an existing N++ plugin never
//! sends them, and adding more here never changes the shape of an
//! existing message (the same ABI-freeze discipline the `NPPM_*` set
//! follows).

/// Base of the Code++ extension message band, `WM_USER + 5000`.
/// Deliberately above both `NPPMSG` and `RUNCOMMAND_USER` so it cannot
/// alias a real Notepad++ message number.
pub const CODEPPMSG: u32 = 0x0400 + 5000;

/// Width of the Code++ extension band the dispatcher claims
/// (`CODEPPMSG..CODEPPMSG + CODEPPMSG_RANGE`). The `wnd_proc`
/// pre-filters must use the same bound as the dispatcher's internal
/// range check — see the equivalent note on [`crate::NPPMSG_RANGE`].
pub const CODEPPMSG_RANGE: u32 = 16;

/// `CODEPPM_EXPORTSAVEDIALOG(0, ExportSaveRequest*)` — hand the host a
/// byte buffer plus a suggested file name; the host defers a native
/// Save-As dialog, writes the bytes to the chosen path, and reports the
/// outcome on the status bar. Fire-and-forget: the dialog runs *after*
/// the dispatch returns (the borrow model forbids an inline modal that
/// returns a result), so the LRESULT is only `1` = accepted / `0` =
/// rejected (bad arguments), never the chosen path.
pub const CODEPPM_EXPORTSAVEDIALOG: u32 = CODEPPMSG;

/// `CODEPPM_SETCLIPBOARD(0, ClipboardSetRequest*)` — place one or more
/// [`ClipEntry`] formats on the system clipboard in a single
/// operation. Runs inline (no nested dialog pump) and returns `1` on
/// success / `0` on failure.
pub const CODEPPM_SETCLIPBOARD: u32 = CODEPPMSG + 1;

// ---- Export "kind" hints (dialog filter + default extension) -------

/// HTML output — `*.html` filter, `html` default extension.
pub const EXPORT_KIND_HTML: u32 = 0;
/// RTF output — `*.rtf` filter, `rtf` default extension.
pub const EXPORT_KIND_RTF: u32 = 1;
/// Anything else — `*.*` filter, no forced extension.
pub const EXPORT_KIND_OTHER: u32 = 2;

// ---- Clipboard formats (abstract; each backend maps to native) -----

/// Plain UTF-8 text. Win32 → `CF_UNICODETEXT` (re-encoded UTF-16);
/// GTK → `text/plain;charset=utf-8` + `UTF8_STRING`.
pub const CLIP_FORMAT_PLAIN: u32 = 0;
/// UTF-8 HTML fragment/document. Win32 → the registered `HTML Format`
/// (wrapped in the `CF_HTML` byte-offset envelope host-side);
/// GTK → `text/html`.
pub const CLIP_FORMAT_HTML: u32 = 1;
/// RTF document bytes (ASCII with `\uN?` escapes). Win32 → the
/// registered `Rich Text Format`; GTK → `text/rtf`.
pub const CLIP_FORMAT_RTF: u32 = 2;

/// Payload for [`CODEPPM_EXPORTSAVEDIALOG`]. A plugin hands the host a
/// byte buffer plus a suggested file name; the host runs a Save-As
/// dialog and writes the bytes. Code++-specific — not part of the
/// Notepad++ ABI.
///
/// The host reads every field once, copies `data` into an owned buffer,
/// and does not retain any pointer past the dispatch call, so the
/// plugin may free its allocation immediately after `SendMessage`
/// returns.
#[repr(C)]
pub struct ExportSaveRequest {
    /// Pointer to `data_len` bytes of file content. May be null only
    /// when `data_len == 0`.
    pub data: *const u8,
    /// Length of `data`, in bytes.
    pub data_len: usize,
    /// Wide-char, null-terminated suggested file name (no directory
    /// component), e.g. `export.html`. Null → the host picks a default.
    pub suggested_name: *const u16,
    /// One of the `EXPORT_KIND_*` constants — selects the dialog's
    /// file-type filter and default extension. The host never
    /// interprets the bytes in `data`.
    pub kind: u32,
}

/// One clipboard payload inside a [`ClipboardSetRequest`]. Code++-only.
#[repr(C)]
pub struct ClipEntry {
    /// One of the `CLIP_FORMAT_*` constants.
    pub format: u32,
    /// Pointer to `data_len` bytes for this format. May be null only
    /// when `data_len == 0` (which the host treats as "skip").
    pub data: *const u8,
    /// Length of `data`, in bytes.
    pub data_len: usize,
}

/// Payload for [`CODEPPM_SETCLIPBOARD`]. Code++-specific — not part of
/// the Notepad++ ABI.
///
/// The host reads `count` [`ClipEntry`] records, copies each format's
/// bytes into owned buffers, and does not retain any plugin pointer
/// past the dispatch call.
#[repr(C)]
pub struct ClipboardSetRequest {
    /// Number of valid [`ClipEntry`] records in `entries`.
    pub count: usize,
    /// Array of `count` [`ClipEntry`] records. May be null only when
    /// `count == 0`.
    pub entries: *const ClipEntry,
}
